//! Our certificate authority for the local cloud (KB: AC — TLS / certificates).
//!
//! An RSA-2048 CA signs the server certificate (hostname as DNS SAN; the
//! module's ECDHE-RSA suites need an RSA key) and the client certificates
//! the module asks for with a CSR. Private keys are secrets: their PEM only
//! leaves this module as a [`Secret`], `Debug` never shows them, and on disk
//! they live in 0600 files under a 0700 directory (see [`Pki::default_dir`]).

use std::fmt;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use openssl::asn1::{Asn1Integer, Asn1Time};
use openssl::bn::{BigNum, MsbOption};
use openssl::hash::MessageDigest;
use openssl::nid::Nid;
use openssl::pkey::{HasPublic, PKey, PKeyRef, Private};
use openssl::rsa::Rsa;
use openssl::x509::extension::{
    AuthorityKeyIdentifier, BasicConstraints, ExtendedKeyUsage, KeyUsage, SubjectAlternativeName,
    SubjectKeyIdentifier,
};
use openssl::x509::{X509, X509Builder, X509NameBuilder, X509NameRef, X509Req};

use crate::error::{ControlError, Result};
use crate::secret::Secret;

const RSA_BITS: u32 = 2048;
const CA_DAYS: u32 = 3650;
const SERVER_DAYS: u32 = 825;
/// A server certificate expiring within this many days is re-issued on load.
const SERVER_RENEW_DAYS: u32 = 30;

/// A key pair and its certificate (the CA or a server leaf).
#[derive(Clone)]
pub struct Identity {
    cert: X509,
    key: PKey<Private>,
}

impl Identity {
    /// The certificate.
    #[must_use]
    pub fn cert(&self) -> &X509 {
        &self.cert
    }

    /// The private key. Never log it.
    #[must_use]
    pub fn key(&self) -> &PKey<Private> {
        &self.key
    }

    /// The certificate as PEM.
    ///
    /// # Errors
    /// [`ControlError::Invalid`] if encoding fails.
    pub fn cert_pem(&self) -> Result<String> {
        pem_string(self.cert.to_pem().map_err(invalid)?)
    }

    /// The private key as PKCS#8 PEM, masked in `Debug`/`Display`.
    ///
    /// # Errors
    /// [`ControlError::Invalid`] if encoding fails.
    pub fn key_pem(&self) -> Result<Secret> {
        let pem = self.key.private_key_to_pem_pkcs8().map_err(invalid)?;
        Ok(Secret::new(pem_string(pem)?))
    }

    /// Parses a certificate and its private key from PEM.
    ///
    /// # Errors
    /// [`ControlError::Invalid`] if either PEM is malformed or the key does
    /// not match the certificate.
    pub fn from_pem(cert_pem: &str, key_pem: &Secret) -> Result<Self> {
        let cert = X509::from_pem(cert_pem.as_bytes()).map_err(invalid)?;
        let key = PKey::private_key_from_pem(key_pem.expose().as_bytes())
            .map_err(|_| ControlError::invalid("private key PEM is malformed"))?;
        let public = cert.public_key().map_err(invalid)?;
        if !public.public_eq(&key) {
            return Err(ControlError::invalid(
                "private key does not match the certificate",
            ));
        }
        Ok(Self { cert, key })
    }

    /// Whether the certificate's DNS SANs include `hostname`.
    #[must_use]
    pub fn has_dns_san(&self, hostname: &str) -> bool {
        self.cert.subject_alt_names().is_some_and(|names| {
            names.iter().any(|n| {
                n.dnsname()
                    .is_some_and(|d| d.eq_ignore_ascii_case(hostname))
            })
        })
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Identity")
            .field("subject", &self.cert.subject_name())
            .field("key", &"***")
            .finish()
    }
}

/// Our certificate authority.
#[derive(Clone, Debug)]
pub struct Ca(Identity);

impl Ca {
    /// Generates a self-signed RSA-2048 CA with common name `common_name`.
    ///
    /// # Errors
    /// [`ControlError::Invalid`] if key generation or signing fails.
    pub fn generate(common_name: &str) -> Result<Self> {
        let key = rsa_key()?;
        let mut name = X509NameBuilder::new().map_err(invalid)?;
        name.append_entry_by_nid(Nid::COMMONNAME, common_name)
            .map_err(invalid)?;
        let name = name.build();

        let mut b = builder(&name, &name, &key, CA_DAYS)?;
        b.append_extension(
            BasicConstraints::new()
                .critical()
                .ca()
                .build()
                .map_err(invalid)?,
        )
        .map_err(invalid)?;
        b.append_extension(
            KeyUsage::new()
                .critical()
                .key_cert_sign()
                .crl_sign()
                .build()
                .map_err(invalid)?,
        )
        .map_err(invalid)?;
        let ski = SubjectKeyIdentifier::new()
            .build(&b.x509v3_context(None, None))
            .map_err(invalid)?;
        b.append_extension(ski).map_err(invalid)?;
        b.sign(&key, MessageDigest::sha256()).map_err(invalid)?;
        Ok(Self(Identity {
            cert: b.build(),
            key,
        }))
    }

    /// Wraps an existing CA identity (for example loaded with [`Identity::from_pem`]).
    #[must_use]
    pub fn from_identity(identity: Identity) -> Self {
        Self(identity)
    }

    /// The CA certificate and key.
    #[must_use]
    pub fn identity(&self) -> &Identity {
        &self.0
    }

    /// The CA certificate.
    #[must_use]
    pub fn cert(&self) -> &X509 {
        &self.0.cert
    }

    /// Issues an RSA-2048 server certificate for `hostname` (DNS SAN and CN).
    ///
    /// # Errors
    /// [`ControlError::Invalid`] if `hostname` is empty or signing fails.
    pub fn issue_server(&self, hostname: &str) -> Result<Identity> {
        if hostname.trim().is_empty() {
            return Err(ControlError::invalid("server hostname is empty"));
        }
        let key = rsa_key()?;
        let mut name = X509NameBuilder::new().map_err(invalid)?;
        name.append_entry_by_nid(Nid::COMMONNAME, hostname)
            .map_err(invalid)?;
        let name = name.build();

        let mut b = builder(&name, self.0.cert.subject_name(), &key, SERVER_DAYS)?;
        self.leaf_extensions(&mut b, ExtendedKeyUsage::new().server_auth())?;
        let san = SubjectAlternativeName::new()
            .dns(hostname)
            .build(&b.x509v3_context(Some(&self.0.cert), None))
            .map_err(invalid)?;
        b.append_extension(san).map_err(invalid)?;
        b.sign(&self.0.key, MessageDigest::sha256())
            .map_err(invalid)?;
        Ok(Identity {
            cert: b.build(),
            key,
        })
    }

    /// Signs a client certificate request (the module's CSR) for `days` days.
    /// Only the CSR's subject and public key are used; its requested
    /// extensions are ignored. Returns the certificate as PEM.
    ///
    /// # Errors
    /// [`ControlError::Invalid`] if the CSR is malformed, its signature does
    /// not verify, or signing fails.
    pub fn sign_csr(&self, csr_pem: &str, days: u32) -> Result<String> {
        let req = X509Req::from_pem(csr_pem.as_bytes()).map_err(invalid)?;
        let public = req.public_key().map_err(invalid)?;
        if !req.verify(&public).map_err(invalid)? {
            return Err(ControlError::invalid("CSR signature does not verify"));
        }
        let mut b = builder(
            req.subject_name(),
            self.0.cert.subject_name(),
            &public,
            days,
        )?;
        self.leaf_extensions(&mut b, ExtendedKeyUsage::new().client_auth())?;
        b.sign(&self.0.key, MessageDigest::sha256())
            .map_err(invalid)?;
        pem_string(b.build().to_pem().map_err(invalid)?)
    }

    fn leaf_extensions(&self, b: &mut X509Builder, eku: &ExtendedKeyUsage) -> Result<()> {
        b.append_extension(
            BasicConstraints::new()
                .critical()
                .build()
                .map_err(invalid)?,
        )
        .map_err(invalid)?;
        b.append_extension(
            KeyUsage::new()
                .critical()
                .digital_signature()
                .key_encipherment()
                .build()
                .map_err(invalid)?,
        )
        .map_err(invalid)?;
        b.append_extension(eku.build().map_err(invalid)?)
            .map_err(invalid)?;
        let aki = AuthorityKeyIdentifier::new()
            .keyid(false)
            .build(&b.x509v3_context(Some(&self.0.cert), None))
            .map_err(invalid)?;
        b.append_extension(aki).map_err(invalid)?;
        Ok(())
    }
}

/// The CA plus the server certificate, persisted as PEM files.
#[derive(Clone, Debug)]
pub struct Pki {
    /// Our CA: signs the server certificate and the module's client certificates.
    pub ca: Ca,
    /// The certificate and key our HTTPS/MQTTS servers present to the module.
    pub server: Identity,
}

const CA_CERT: &str = "ca.pem";
const CA_KEY: &str = "ca.key";
const SERVER_CERT: &str = "server.pem";
const SERVER_KEY: &str = "server.key";

impl Pki {
    /// `$HOME/Library/Application Support/Controlz/pki`.
    ///
    /// # Errors
    /// [`ControlError::Config`] if the home directory cannot be found.
    pub fn default_dir() -> Result<PathBuf> {
        let home = std::env::home_dir()
            .ok_or_else(|| ControlError::config("cannot find the home directory"))?;
        Ok(home
            .join("Library")
            .join("Application Support")
            .join("Controlz")
            .join("pki"))
    }

    /// Loads the CA and server certificate from `dir`, generating whatever is
    /// missing. Anything newly generated is saved.
    ///
    /// The server certificate is re-issued when the CA was just generated, or
    /// when it lacks `hostname` in its SANs, was not signed by the CA, expires
    /// within 30 days, or its files are unreadable as a pair (for example a
    /// new key next to the old cert after a crash). A bad CA pair is a hard
    /// error instead: regenerating it silently would orphan enrolled modules.
    ///
    /// # Errors
    /// [`ControlError::Config`] on file I/O errors, [`ControlError::Invalid`]
    /// on malformed CA files or generation failures.
    pub fn load_or_create(dir: &Path, hostname: &str) -> Result<Self> {
        let (ca, ca_is_new) = if let Some(id) = load_identity(dir, CA_CERT, CA_KEY)? {
            (Ca(id), false)
        } else {
            tracing::info!(dir = %dir.display(), "generating a new Controlz CA");
            let ca = Ca::generate("Controlz local CA")?;
            write_pair(dir, CA_CERT, CA_KEY, ca.identity())?;
            (ca, true)
        };
        let existing = match load_identity(dir, SERVER_CERT, SERVER_KEY) {
            Ok(id) => id,
            Err(ControlError::Invalid { message }) => {
                tracing::warn!(%message, "server certificate files unusable; re-issuing");
                None
            }
            Err(e) => return Err(e),
        };
        let server = match existing {
            Some(id) if !ca_is_new && server_is_current(&id, &ca, hostname)? => id,
            _ => {
                tracing::info!(hostname, "issuing a new server certificate");
                let id = ca.issue_server(hostname)?;
                write_pair(dir, SERVER_CERT, SERVER_KEY, &id)?;
                id
            }
        };
        Ok(Self { ca, server })
    }

    /// Writes all four PEM files to `dir` (created 0700; keys 0600).
    ///
    /// # Errors
    /// [`ControlError::Config`] on file I/O errors.
    pub fn save(&self, dir: &Path) -> Result<()> {
        write_pair(dir, CA_CERT, CA_KEY, self.ca.identity())?;
        write_pair(dir, SERVER_CERT, SERVER_KEY, &self.server)
    }
}

/// Whether a loaded server identity can be kept: right SAN, signed by `ca`,
/// and not expiring within [`SERVER_RENEW_DAYS`].
fn server_is_current(id: &Identity, ca: &Ca, hostname: &str) -> Result<bool> {
    let ca_key = ca.cert().public_key().map_err(invalid)?;
    let renew_by = Asn1Time::days_from_now(SERVER_RENEW_DAYS).map_err(invalid)?;
    Ok(id.has_dns_san(hostname)
        && id.cert().verify(&ca_key).map_err(invalid)?
        && id.cert().not_after() > renew_by)
}

fn load_identity(dir: &Path, cert_file: &str, key_file: &str) -> Result<Option<Identity>> {
    let key_path = dir.join(key_file);
    let (Some(cert), Some(key)) = (read_opt(&dir.join(cert_file))?, read_opt(&key_path)?) else {
        return Ok(None);
    };
    // A key copied in by hand may be world-readable; tighten it on every load.
    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600))
        .map_err(|e| ControlError::config(format!("{}: {e}", key_path.display())))?;
    Identity::from_pem(&cert, &Secret::new(key))
        .map(Some)
        .map_err(|e| ControlError::invalid(format!("{}: {e}", dir.join(cert_file).display())))
}

fn read_opt(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ControlError::config(format!("{}: {e}", path.display()))),
    }
}

fn write_pair(dir: &Path, cert_file: &str, key_file: &str, id: &Identity) -> Result<()> {
    let io_err = |e: std::io::Error| ControlError::config(format!("{}: {e}", dir.display()));
    fs::create_dir_all(dir).map_err(io_err)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(io_err)?;
    // Key first. On first creation a crash leaves a key without its cert,
    // which reloads as missing. On server re-issue it can leave the new key
    // next to the old cert; `load_or_create` treats that pair as unusable and
    // re-issues. The CA pair is only (re)written with unchanged contents
    // after creation, so a crash cannot mismatch it.
    write_atomic(&dir.join(key_file), id.key_pem()?.expose(), 0o600)?;
    write_atomic(&dir.join(cert_file), &id.cert_pem()?, 0o644)
}

/// Temp file (pid-unique, created with `mode`) + rename, like `Config::save_to`.
fn write_atomic(path: &Path, text: &str, mode: u32) -> Result<()> {
    let io_err = |e: std::io::Error| ControlError::config(format!("{}: {e}", path.display()));
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let written = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .and_then(|()| fs::rename(&tmp, path));
    if let Err(e) = written {
        // Best effort: the original error matters more than cleanup failing.
        let _ = fs::remove_file(&tmp);
        return Err(io_err(e));
    }
    Ok(())
}

fn rsa_key() -> Result<PKey<Private>> {
    let rsa = Rsa::generate(RSA_BITS).map_err(invalid)?;
    PKey::from_rsa(rsa).map_err(invalid)
}

fn serial() -> Result<Asn1Integer> {
    let mut bn = BigNum::new().map_err(invalid)?;
    bn.rand(127, MsbOption::MAYBE_ZERO, false)
        .map_err(invalid)?;
    bn.to_asn1_integer().map_err(invalid)
}

/// A v3 certificate builder with a random serial and `days` of validity from now.
fn builder<T: HasPublic>(
    subject: &X509NameRef,
    issuer: &X509NameRef,
    key: &PKeyRef<T>,
    days: u32,
) -> Result<X509Builder> {
    let mut b = X509Builder::new().map_err(invalid)?;
    b.set_version(2).map_err(invalid)?;
    let serial: Asn1Integer = serial()?;
    let not_before = Asn1Time::days_from_now(0).map_err(invalid)?;
    let not_after = Asn1Time::days_from_now(days).map_err(invalid)?;
    b.set_serial_number(&serial).map_err(invalid)?;
    b.set_subject_name(subject).map_err(invalid)?;
    b.set_issuer_name(issuer).map_err(invalid)?;
    b.set_pubkey(key).map_err(invalid)?;
    b.set_not_before(&not_before).map_err(invalid)?;
    b.set_not_after(&not_after).map_err(invalid)?;
    Ok(b)
}

fn pem_string(bytes: Vec<u8>) -> Result<String> {
    String::from_utf8(bytes).map_err(invalid)
}

fn invalid(e: impl fmt::Display) -> ControlError {
    ControlError::invalid(format!("PKI: {e}"))
}

/// One CA + server certificate shared by every test (RSA generation is slow).
#[cfg(test)]
pub(crate) mod test_fixture {
    use super::{Ca, Pki};
    use std::sync::LazyLock;

    pub(crate) const HOSTNAME: &str = "controlz.test";

    pub(crate) static FIXTURE: LazyLock<Pki> = LazyLock::new(|| {
        let ca = Ca::generate("Controlz test CA").unwrap();
        let server = ca.issue_server(HOSTNAME).unwrap();
        Pki { ca, server }
    });
}

#[cfg(test)]
mod tests {
    use super::test_fixture::{FIXTURE, HOSTNAME};
    use super::*;
    use openssl::ec::{EcGroup, EcKey};
    use openssl::stack::Stack;
    use openssl::x509::X509ReqBuilder;
    use openssl::x509::store::X509StoreBuilder;
    use openssl::x509::{X509StoreContext, X509VerifyResult};
    use std::io;
    use std::sync::{Arc, Mutex};

    fn verifies_against_ca(cert: &X509) -> bool {
        let ca_key = FIXTURE.ca.cert().public_key().unwrap();
        assert!(cert.verify(&ca_key).unwrap(), "signature");
        let mut store = X509StoreBuilder::new().unwrap();
        store.add_cert(FIXTURE.ca.cert().clone()).unwrap();
        let store = store.build();
        let mut ctx = X509StoreContext::new().unwrap();
        let chain = Stack::new().unwrap();
        ctx.init(&store, cert, &chain, |c| {
            let ok = c.verify_cert()?;
            assert_eq!(c.error(), X509VerifyResult::OK);
            Ok(ok)
        })
        .unwrap()
    }

    #[test]
    fn signed_csr_verifies_against_ca() {
        // EC key: cheap to generate, and sign_csr must not care about the type.
        let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
        let key = PKey::from_ec_key(EcKey::generate(&group).unwrap()).unwrap();
        let mut name = X509NameBuilder::new().unwrap();
        name.append_entry_by_nid(Nid::COMMONNAME, "fake-device-uuid")
            .unwrap();
        let name = name.build();
        let mut req = X509ReqBuilder::new().unwrap();
        req.set_subject_name(&name).unwrap();
        req.set_pubkey(&key).unwrap();
        req.sign(&key, MessageDigest::sha256()).unwrap();
        let csr = String::from_utf8(req.build().to_pem().unwrap()).unwrap();

        let cert = X509::from_pem(FIXTURE.ca.sign_csr(&csr, 365).unwrap().as_bytes()).unwrap();
        assert!(verifies_against_ca(&cert));
        assert!(cert.public_key().unwrap().public_eq(&key));
        let cn = cert.subject_name().entries_by_nid(Nid::COMMONNAME).next();
        assert_eq!(cn.unwrap().data().as_slice(), b"fake-device-uuid");
    }

    #[test]
    fn server_cert_carries_san() {
        assert!(FIXTURE.server.has_dns_san(HOSTNAME));
        assert!(!FIXTURE.server.has_dns_san("other.test"));
        assert!(verifies_against_ca(FIXTURE.server.cert()));
    }

    #[test]
    fn saved_files_are_private_and_reload() {
        let dir = std::env::temp_dir().join(format!("controlz-pki-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        FIXTURE.save(&dir).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(CA_KEY)), 0o600);
        assert_eq!(mode(&dir.join(SERVER_KEY)), 0o600);

        let loaded = Pki::load_or_create(&dir, HOSTNAME).unwrap();
        assert_eq!(
            loaded.server.cert().to_der().unwrap(),
            FIXTURE.server.cert().to_der().unwrap(),
            "existing files must be reused"
        );
        assert!(
            loaded
                .ca
                .identity()
                .key()
                .public_eq(FIXTURE.ca.identity().key())
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn new_ca_reissues_the_server_cert() {
        let dir = std::env::temp_dir().join(format!("controlz-pki-newca-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        FIXTURE.save(&dir).unwrap();
        fs::remove_file(dir.join(CA_CERT)).unwrap();
        fs::remove_file(dir.join(CA_KEY)).unwrap();

        let loaded = Pki::load_or_create(&dir, HOSTNAME).unwrap();
        let new_ca_key = loaded.ca.cert().public_key().unwrap();
        assert!(!new_ca_key.public_eq(FIXTURE.ca.identity().key()));
        assert!(loaded.server.cert().verify(&new_ca_key).unwrap());
        assert!(loaded.server.has_dns_san(HOSTNAME));
        assert!(server_is_current(&loaded.server, &loaded.ca, HOSTNAME).unwrap());
        assert!(!server_is_current(&FIXTURE.server, &loaded.ca, HOSTNAME).unwrap());
        fs::remove_dir_all(&dir).unwrap();
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
    fn private_keys_do_not_leak() {
        let key_pem = FIXTURE.server.key_pem().unwrap();
        // A line from the middle of the PEM body: pure key material.
        let body_line = key_pem.expose().lines().nth(5).unwrap().to_owned();

        let buffer = Buffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(pki = ?*FIXTURE, key = %key_pem, "pki loaded");
        });
        let logs = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();

        for out in [logs, format!("{:#?}", *FIXTURE), format!("{key_pem:?}")] {
            assert!(out.contains("***"), "missing mask: {out}");
            assert!(!out.contains("PRIVATE KEY"), "key leaked: {out}");
            assert!(!out.contains(&body_line), "key leaked: {out}");
        }
    }
}

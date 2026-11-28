//! Our certificate authority for the local cloud (KB: AC — TLS / certificates).
//!
//! Two RSA-2048 CAs, both `pathlen:0`: the server CA signs only the server
//! certificate (hostname as DNS SAN, or IP SAN for an IP; the module's ECDHE-RSA suites need an
//! RSA key) and is the CA the module is told to trust; the client CA signs
//! only the client certificates the module asks for with a CSR, and is never
//! handed to the module. So a client certificate can never pass as our
//! server certificate, even if the module ignores EKU. Private keys are secrets: their PEM only
//! leaves this module as a [`Secret`], `Debug` never shows them, and on disk
//! they live in 0600 files under a 0700 directory (see [`Pki::default_dir`]).

use std::fmt;
use std::fs;
use std::io::Write as _;
use std::net::IpAddr;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use openssl::asn1::{Asn1Integer, Asn1Time};
use openssl::bn::{BigNum, MsbOption};
use openssl::hash::MessageDigest;
use openssl::nid::Nid;
use openssl::pkey::{HasPublic, Id, PKey, PKeyRef, Private, Public};
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
/// Longest validity [`Ca::sign_csr`] grants (the CA/B 825-day ceiling).
pub const MAX_CLIENT_DAYS: u32 = 825;
/// Smallest RSA key [`Ca::sign_csr`] accepts.
const MIN_CSR_RSA_BITS: u32 = 2048;
/// Largest RSA key [`Ca::sign_csr`] accepts (bounds the verify cost).
const MAX_CSR_RSA_BITS: u32 = 8192;

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

    /// Whether the certificate's SANs cover `host`: an IP SAN with the same
    /// address when `host` is an IP address, otherwise a DNS SAN equal to it
    /// (ASCII case-insensitive).
    #[must_use]
    pub fn has_san(&self, host: &str) -> bool {
        let ip = host.parse::<IpAddr>().ok().map(|ip| ip_octets(&ip));
        self.cert.subject_alt_names().is_some_and(|names| {
            names.iter().any(|n| match &ip {
                Some(octets) => n.ipaddress() == Some(octets.as_slice()),
                None => n.dnsname().is_some_and(|d| d.eq_ignore_ascii_case(host)),
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
    /// Its basic constraints carry `pathlen:0`: it can sign leaves only.
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
                .pathlen(0)
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

    /// Issues an RSA-2048 server certificate for `hostname` (its CN). The SAN
    /// is an IP SAN when `hostname` is an IP address (the module is then
    /// pointed at a bare IP), otherwise a DNS SAN.
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
        let mut san = SubjectAlternativeName::new();
        if hostname.parse::<IpAddr>().is_ok() {
            san.ip(hostname);
        } else {
            san.dns(hostname);
        }
        let san = san
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
    /// Call this on [`Pki::client_ca`] only, never on the server CA.
    ///
    /// # Errors
    /// [`ControlError::Invalid`] if `days` is not in `1..=`[`MAX_CLIENT_DAYS`],
    /// the CSR is malformed, its key is neither RSA of 2048 to 8192 bits nor
    /// EC P-256, its signature does not verify, its subject does not have
    /// exactly one common name equal to `expected_cn`, or signing fails.
    /// Error messages never include the CSR or the common name.
    pub fn sign_csr(&self, csr_pem: &str, expected_cn: &str, days: u32) -> Result<String> {
        if !(1..=MAX_CLIENT_DAYS).contains(&days) {
            return Err(ControlError::invalid(format!(
                "client certificate validity must be 1 to {MAX_CLIENT_DAYS} days"
            )));
        }
        let req = X509Req::from_pem(csr_pem.as_bytes()).map_err(invalid)?;
        let public = req.public_key().map_err(invalid)?;
        check_csr_key(&public)?;
        if !req.verify(&public).map_err(invalid)? {
            return Err(ControlError::invalid("CSR signature does not verify"));
        }
        let mut cns = req.subject_name().entries_by_nid(Nid::COMMONNAME);
        let cn_matches = cns
            .next()
            .is_some_and(|cn| cn.data().as_slice() == expected_cn.as_bytes());
        if !cn_matches || cns.next().is_some() {
            return Err(ControlError::invalid(
                "CSR subject must have exactly one common name, equal to the device uuid",
            ));
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

/// Accepts RSA keys of [`MIN_CSR_RSA_BITS`]..=[`MAX_CSR_RSA_BITS`] and EC P-256.
fn check_csr_key(key: &PKeyRef<Public>) -> Result<()> {
    let ok = match key.id() {
        Id::RSA => (MIN_CSR_RSA_BITS..=MAX_CSR_RSA_BITS).contains(&key.bits()),
        Id::EC => key
            .ec_key()
            .map_err(invalid)?
            .group()
            .curve_name()
            .is_some_and(|nid| nid == Nid::X9_62_PRIME256V1),
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(ControlError::invalid(
            "CSR key must be RSA of 2048 to 8192 bits or EC P-256",
        ))
    }
}

/// Both CAs plus the server certificate, persisted as PEM files.
#[derive(Clone, Debug)]
pub struct Pki {
    /// The server CA: signs only the server certificate. This is the CA
    /// `/route/certificate` hands to the module.
    pub ca: Ca,
    /// The client CA: signs only the module's CSRs. Never sent to the module.
    pub client_ca: Ca,
    /// The certificate and key our HTTPS/MQTTS servers present to the module.
    pub server: Identity,
}

const CA_CERT: &str = "ca.pem";
const CA_KEY: &str = "ca.key";
const CLIENT_CA_CERT: &str = "client-ca.pem";
const CLIENT_CA_KEY: &str = "client-ca.key";
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

    /// Loads both CAs and the server certificate from `dir`, generating
    /// whatever is missing. Anything newly generated is saved.
    ///
    /// The server certificate is re-issued when the CA was just generated, or
    /// when it lacks `hostname` in its SANs, was not signed by the CA, expires
    /// within 30 days, or its files are unreadable as a pair (for example a
    /// new key next to the old cert after a crash). A bad pair for either CA
    /// is a hard error instead: regenerating it silently would orphan
    /// enrolled modules.
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
        let client_ca = if let Some(id) = load_identity(dir, CLIENT_CA_CERT, CLIENT_CA_KEY)? {
            Ca(id)
        } else {
            tracing::info!(dir = %dir.display(), "generating a new Controlz client CA");
            let ca = Ca::generate("Controlz local client CA")?;
            write_pair(dir, CLIENT_CA_CERT, CLIENT_CA_KEY, ca.identity())?;
            ca
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
                tracing::info!("issuing a new server certificate");
                tracing::debug!(hostname, "server certificate hostname");
                let id = ca.issue_server(hostname)?;
                write_pair(dir, SERVER_CERT, SERVER_KEY, &id)?;
                id
            }
        };
        Ok(Self {
            ca,
            client_ca,
            server,
        })
    }

    /// Writes all six PEM files to `dir` (created 0700; keys 0600).
    ///
    /// # Errors
    /// [`ControlError::Config`] on file I/O errors.
    pub fn save(&self, dir: &Path) -> Result<()> {
        write_pair(dir, CA_CERT, CA_KEY, self.ca.identity())?;
        write_pair(
            dir,
            CLIENT_CA_CERT,
            CLIENT_CA_KEY,
            self.client_ca.identity(),
        )?;
        write_pair(dir, SERVER_CERT, SERVER_KEY, &self.server)
    }
}

/// The pinned public keys of modules whose client certificate is not ours
/// (D21: the module keeps the LG-issued certificate from its original
/// `ThinQ` registration and never sends a CSR).
///
/// One small file per module in the PKI directory, named
/// `module-pin-<first 16 hex of SHA-256(uuid)>.txt` so the uuid never
/// appears in a file name, holding the lowercase hex SHA-256 of the
/// certificate's `SubjectPublicKeyInfo`. Written 0600 once, create-exclusive
/// (temp + hard link); never replaced by the broker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModulePins {
    dir: PathBuf,
}

/// Hex length of a SHA-256 digest.
const SHA256_HEX: usize = 64;

impl ModulePins {
    /// Pins stored in `dir` (normally [`Pki::default_dir`]).
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn path(&self, uuid: &str) -> PathBuf {
        let digest = openssl::sha::sha256(uuid.as_bytes());
        let short = hex::encode(&digest[..8]);
        self.dir.join(format!("module-pin-{short}.txt"))
    }

    /// The pinned SPKI SHA-256 (lowercase hex) for `uuid`, if any.
    ///
    /// # Errors
    /// [`ControlError::Config`] on I/O errors; [`ControlError::Invalid`] if
    /// the file does not hold one SHA-256 hex digest.
    pub fn get(&self, uuid: &str) -> Result<Option<String>> {
        let path = self.path(uuid);
        let Some(text) = read_opt(&path)? else {
            return Ok(None);
        };
        let pin = text.trim().to_ascii_lowercase();
        if pin.len() != SHA256_HEX || !pin.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ControlError::invalid(format!(
                "{}: not a SHA-256 pin; delete it to re-pin",
                path.display()
            )));
        }
        Ok(Some(pin))
    }

    /// Pins `spki_sha256` (lowercase hex) for `uuid` unless a pin already
    /// exists, and returns the pin now stored: the new one, or the existing
    /// one (possibly different) untouched. Never replaces a pin file, even
    /// against another process: the file is written under a temp name and
    /// then hard-linked to the final name, which fails if that name exists.
    /// Creates the directory 0700; the file is 0600.
    ///
    /// # Errors
    /// [`ControlError::Invalid`] if `spki_sha256` is not a SHA-256 hex
    /// digest or the existing file is not a pin; [`ControlError::Config`] on
    /// I/O errors.
    pub fn pin_first(&self, uuid: &str, spki_sha256: &str) -> Result<String> {
        if spki_sha256.len() != SHA256_HEX || !spki_sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ControlError::invalid(
                "module pin is not a SHA-256 hex digest",
            ));
        }
        let io_err =
            |e: std::io::Error| ControlError::config(format!("{}: {e}", self.dir.display()));
        fs::create_dir_all(&self.dir).map_err(io_err)?;
        fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700)).map_err(io_err)?;
        let pin = spki_sha256.to_ascii_lowercase();
        let path = self.path(uuid);
        if link_exclusive(&path, &format!("{pin}\n"), 0o600)? {
            return Ok(pin);
        }
        self.get(uuid)?.ok_or_else(|| {
            ControlError::config(format!("{}: vanished while pinning", path.display()))
        })
    }
}

/// Writes `text` to a temp file (pid-unique, created with `mode`) and
/// hard-links it to `path`. `Ok(false)` if `path` already exists, which is
/// left as it was. The temp file is removed once created; one that already
/// exists (a leftover) is an error naming it, never "already pinned".
fn link_exclusive(path: &Path, text: &str, mode: u32) -> Result<bool> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let tmp_err = |e: std::io::Error| ControlError::config(format!("{}: {e}", tmp.display()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)
        .map_err(tmp_err)?;
    let written = file
        .write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(tmp_err);
    drop(file);
    let linked = written.map(|()| fs::hard_link(&tmp, path));
    // Best effort: the link (or the original error) matters more than cleanup.
    let _ = fs::remove_file(&tmp);
    match linked? {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(ControlError::config(format!("{}: {e}", path.display()))),
    }
}

/// Whether a loaded server identity can be kept: right SAN, signed by `ca`,
/// and not expiring within [`SERVER_RENEW_DAYS`]. A signature check that
/// errors (for example a key type that does not match the CA's) counts as
/// "not signed by `ca`", so the caller re-issues instead of failing.
fn server_is_current(id: &Identity, ca: &Ca, hostname: &str) -> Result<bool> {
    let ca_key = ca.cert().public_key().map_err(invalid)?;
    let renew_by = Asn1Time::days_from_now(SERVER_RENEW_DAYS).map_err(invalid)?;
    let signed_by_ca = match id.cert().verify(&ca_key) {
        Ok(ok) => ok,
        Err(e) => {
            tracing::warn!(error = %e, "server certificate signature check failed; re-issuing");
            false
        }
    };
    Ok(id.has_san(hostname) && signed_by_ca && id.cert().not_after() > renew_by)
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

/// The address bytes as an X.509 IP SAN stores them (4 or 16 bytes).
fn ip_octets(ip: &IpAddr) -> Vec<u8> {
    match ip {
        IpAddr::V4(v4) => v4.octets().to_vec(),
        IpAddr::V6(v6) => v6.octets().to_vec(),
    }
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

/// Both CAs + server certificate shared by every test (RSA generation is slow).
#[cfg(test)]
pub(crate) mod test_fixture {
    use super::{Ca, Pki};
    use openssl::bn::{BigNum, BigNumRef};
    use openssl::ec::{EcGroup, EcKey};
    use openssl::hash::MessageDigest;
    use openssl::nid::Nid;
    use openssl::pkey::{PKey, Private};
    use openssl::rsa::Rsa;
    use openssl::stack::Stack;
    use openssl::x509::store::X509StoreBuilder;
    use openssl::x509::{
        X509, X509NameBuilder, X509ReqBuilder, X509StoreContext, X509StoreContextRef,
    };
    use std::sync::LazyLock;

    pub(crate) const HOSTNAME: &str = "controlz.test";

    pub(crate) static FIXTURE: LazyLock<Pki> = LazyLock::new(|| {
        let ca = Ca::generate("Controlz test CA").unwrap();
        let client_ca = Ca::generate("Controlz test client CA").unwrap();
        let server = ca.issue_server(HOSTNAME).unwrap();
        Pki {
            ca,
            client_ca,
            server,
        }
    });

    /// A CA that is not ours, standing in for LG's / AWS's issuer (D21).
    pub(crate) static FOREIGN_CA: LazyLock<Ca> =
        LazyLock::new(|| Ca::generate("Fake LG test CA").unwrap());

    /// An RSA key whose public half equals `key`'s but whose private
    /// exponents are wrong: OpenSSL takes it as `key`'s pair (the public
    /// parts match) yet every signature it makes is invalid.
    pub(crate) fn mismatched_rsa_key(key: &PKey<Private>) -> PKey<Private> {
        let rsa = key.rsa().unwrap();
        let two = BigNum::from_u32(2).unwrap();
        let bump = |n: &BigNumRef| {
            let mut out = BigNum::new().unwrap();
            out.checked_add(n, &two).unwrap();
            out
        };
        let broken = Rsa::from_private_components(
            rsa.n().to_owned().unwrap(),
            rsa.e().to_owned().unwrap(),
            bump(rsa.d()),
            rsa.p().unwrap().to_owned().unwrap(),
            rsa.q().unwrap().to_owned().unwrap(),
            bump(rsa.dmp1().unwrap()),
            bump(rsa.dmq1().unwrap()),
            rsa.iqmp().unwrap().to_owned().unwrap(),
        )
        .unwrap();
        let broken = PKey::from_rsa(broken).unwrap();
        assert!(broken.public_eq(key), "same public key");
        broken
    }

    /// A fresh EC P-256 key (cheap to generate, unlike RSA).
    pub(crate) fn p256_key() -> PKey<Private> {
        let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
        PKey::from_ec_key(EcKey::generate(&group).unwrap()).unwrap()
    }

    /// A PEM CSR signed by `key` whose subject has one CN entry per `cns`.
    pub(crate) fn csr_pem(key: &PKey<Private>, cns: &[&str]) -> String {
        let mut name = X509NameBuilder::new().unwrap();
        for cn in cns {
            name.append_entry_by_nid(Nid::COMMONNAME, cn).unwrap();
        }
        let name = name.build();
        let mut req = X509ReqBuilder::new().unwrap();
        req.set_subject_name(&name).unwrap();
        req.set_pubkey(key).unwrap();
        req.sign(key, MessageDigest::sha256()).unwrap();
        String::from_utf8(req.build().to_pem().unwrap()).unwrap()
    }

    /// Whether `cert` chains to the CA certificate `ca` (full X.509 path check).
    pub(crate) fn chains_to(cert: &X509, ca: &X509) -> bool {
        let mut store = X509StoreBuilder::new().unwrap();
        store.add_cert(ca.clone()).unwrap();
        let store = store.build();
        let mut ctx = X509StoreContext::new().unwrap();
        let chain = Stack::new().unwrap();
        ctx.init(&store, cert, &chain, X509StoreContextRef::verify_cert)
            .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::test_fixture::{FIXTURE, HOSTNAME, chains_to, csr_pem, p256_key};
    use super::*;
    use openssl::ec::{EcGroup, EcKey};
    use std::io;
    use std::sync::{Arc, Mutex};

    const UUID: &str = "fake-device-uuid";

    fn sign(csr: &str) -> Result<String> {
        FIXTURE.client_ca.sign_csr(csr, UUID, 365)
    }

    fn assert_invalid(result: &Result<String>, what: &str) {
        assert!(
            matches!(result, Err(ControlError::Invalid { .. })),
            "{what} must be rejected: {result:?}"
        );
    }

    #[test]
    fn signed_csr_chains_to_the_client_ca_only() {
        let key = p256_key();
        let cert = X509::from_pem(sign(&csr_pem(&key, &[UUID])).unwrap().as_bytes()).unwrap();
        assert!(chains_to(&cert, FIXTURE.client_ca.cert()));
        assert!(
            !chains_to(&cert, FIXTURE.ca.cert()),
            "a client cert must not chain to the server CA"
        );
        assert!(cert.public_key().unwrap().public_eq(&key));
        let cn = cert.subject_name().entries_by_nid(Nid::COMMONNAME).next();
        assert_eq!(cn.unwrap().data().as_slice(), UUID.as_bytes());
    }

    #[test]
    fn cas_have_pathlen_zero() {
        assert_eq!(FIXTURE.ca.cert().pathlen(), Some(0));
        assert_eq!(FIXTURE.client_ca.cert().pathlen(), Some(0));
    }

    #[test]
    fn sign_csr_caps_validity() {
        let csr = csr_pem(&p256_key(), &[UUID]);
        let ca = &FIXTURE.client_ca;
        for days in [1, 365, MAX_CLIENT_DAYS] {
            let pem = ca.sign_csr(&csr, UUID, days).unwrap();
            let cert = X509::from_pem(pem.as_bytes()).unwrap();
            let not_after = cert.not_after();
            let earliest = Asn1Time::days_from_now(days - 1).unwrap();
            let latest = Asn1Time::days_from_now(days + 1).unwrap();
            assert!(
                earliest < not_after && not_after < latest,
                "{days} days: not_after {not_after}"
            );
        }
        assert_invalid(&ca.sign_csr(&csr, UUID, MAX_CLIENT_DAYS + 1), "826 days");
        assert_invalid(&ca.sign_csr(&csr, UUID, 0), "0 days");
    }

    /// A CSR carrying an RSA public key with a `bits`-bit modulus, signed by
    /// an unrelated P-256 key. Generating real 8192+-bit keys is slow, and
    /// the key-size check runs before the signature check, so the error
    /// message tells whether the size check refused it.
    fn rsa_public_csr(bits: i32) -> String {
        let mut n = BigNum::new().unwrap();
        n.rand(bits, MsbOption::ONE, true).unwrap();
        let e = BigNum::from_u32(65537).unwrap();
        let public = PKey::from_rsa(Rsa::from_public_components(n, e).unwrap()).unwrap();
        let mut name = X509NameBuilder::new().unwrap();
        name.append_entry_by_nid(Nid::COMMONNAME, UUID).unwrap();
        let mut req = openssl::x509::X509ReqBuilder::new().unwrap();
        req.set_subject_name(&name.build()).unwrap();
        req.set_pubkey(&public).unwrap();
        req.sign(&p256_key(), MessageDigest::sha256()).unwrap();
        String::from_utf8(req.build().to_pem().unwrap()).unwrap()
    }

    fn invalid_message(result: Result<String>) -> String {
        match result {
            Err(ControlError::Invalid { message }) => message,
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn sign_csr_accepts_rsa_2048_and_caps_rsa_at_8192_bits() {
        let rsa2048 = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
        assert!(sign(&csr_pem(&rsa2048, &[UUID])).is_ok());
        let at_max = invalid_message(sign(&rsa_public_csr(8192)));
        // Past the key check; refused later by the (mismatched) signature check.
        assert!(!at_max.contains("CSR key must be"), "{at_max}");
        let over_max = invalid_message(sign(&rsa_public_csr(8200)));
        assert!(over_max.contains("CSR key must be RSA"), "{over_max}");
    }

    #[test]
    fn sign_csr_rejects_weak_or_unusual_keys() {
        let rsa1024 = PKey::from_rsa(Rsa::generate(1024).unwrap()).unwrap();
        assert_invalid(&sign(&csr_pem(&rsa1024, &[UUID])), "RSA-1024");
        let p384 = EcGroup::from_curve_name(Nid::SECP384R1).unwrap();
        let p384 = PKey::from_ec_key(EcKey::generate(&p384).unwrap()).unwrap();
        assert_invalid(&sign(&csr_pem(&p384, &[UUID])), "P-384");
        let ed25519 = PKey::generate_ed25519().unwrap();
        let mut name = X509NameBuilder::new().unwrap();
        name.append_entry_by_nid(Nid::COMMONNAME, UUID).unwrap();
        let mut req = openssl::x509::X509ReqBuilder::new().unwrap();
        req.set_subject_name(&name.build()).unwrap();
        req.set_pubkey(&ed25519).unwrap();
        req.sign(&ed25519, MessageDigest::null()).unwrap();
        let csr = String::from_utf8(req.build().to_pem().unwrap()).unwrap();
        assert_invalid(&sign(&csr), "Ed25519");
    }

    #[test]
    fn sign_csr_requires_the_expected_cn() {
        let key = p256_key();
        assert_invalid(&sign(&csr_pem(&key, &["other-uuid"])), "CN mismatch");
        assert_invalid(&sign(&csr_pem(&key, &[])), "no CN");
        assert_invalid(&sign(&csr_pem(&key, &[UUID, UUID])), "two CNs");
        assert_invalid(&sign(&csr_pem(&key, &[&format!("{UUID}x")])), "CN prefix");
    }

    #[test]
    fn sign_csr_rejects_a_bad_signature_and_garbage() {
        // A CSR whose subject was signed by another key.
        let (signer, claimed) = (p256_key(), p256_key());
        let mut name = X509NameBuilder::new().unwrap();
        name.append_entry_by_nid(Nid::COMMONNAME, UUID).unwrap();
        let mut req = openssl::x509::X509ReqBuilder::new().unwrap();
        req.set_subject_name(&name.build()).unwrap();
        req.set_pubkey(&claimed).unwrap();
        req.sign(&signer, MessageDigest::sha256()).unwrap();
        let csr = String::from_utf8(req.build().to_pem().unwrap()).unwrap();
        assert_invalid(&sign(&csr), "a forged CSR");
        assert_invalid(&sign("not a csr"), "garbage");
    }

    #[test]
    fn server_cert_carries_san() {
        assert!(FIXTURE.server.has_san(HOSTNAME));
        assert!(FIXTURE.server.has_san("CONTROLZ.test"));
        assert!(!FIXTURE.server.has_san("other.test"));
        assert!(!FIXTURE.server.has_san("192.0.2.10"));
        assert!(chains_to(FIXTURE.server.cert(), FIXTURE.ca.cert()));
        assert!(!chains_to(FIXTURE.server.cert(), FIXTURE.client_ca.cert()));
    }

    #[test]
    fn ip_host_gets_an_ip_san() {
        let v4 = FIXTURE.ca.issue_server("192.0.2.10").unwrap(); // privacy:allow (TEST-NET-1)
        assert!(v4.has_san("192.0.2.10")); // privacy:allow (TEST-NET-1)
        assert!(!v4.has_san("192.0.2.11")); // privacy:allow (TEST-NET-1)
        let names = v4.cert().subject_alt_names().unwrap();
        assert_eq!(names.len(), 1);
        assert_eq!(
            names.get(0).unwrap().ipaddress(),
            Some(&[192, 0, 2, 10][..])
        );
        assert_eq!(
            names.get(0).unwrap().dnsname(),
            None,
            "no DNS SAN for an IP"
        );

        let v6 = FIXTURE.ca.issue_server("2001:db8::1").unwrap();
        assert!(v6.has_san("2001:db8::1"));
        assert!(
            v6.has_san("2001:0db8:0:0:0:0:0:1"),
            "same address, other spelling"
        );
        assert!(!v6.has_san("2001:db8::2"));
        assert!(!FIXTURE.server.has_san("2001:db8::1"));
    }

    /// A client that verifies the chain and the IP SAN, as the module would
    /// when `/route` hands it a bare IP.
    #[tokio::test]
    async fn client_verifies_the_server_by_ip() {
        use openssl::ssl::{SslConnector, SslMethod, SslVersion};
        use std::pin::Pin;
        use tokio::net::{TcpListener, TcpStream};

        let id = FIXTURE.ca.issue_server("127.0.0.1").unwrap();
        let acceptor = crate::ac::tls::server_acceptor(id.cert(), id.key()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (tcp, _) = listener.accept().await.unwrap();
                let _ = crate::ac::tls::accept(&acceptor, tcp).await;
            }
        });
        let connect = |host: &'static str| async move {
            let mut b = SslConnector::builder(SslMethod::tls_client()).unwrap();
            b.set_min_proto_version(Some(SslVersion::TLS1_2)).unwrap();
            b.set_max_proto_version(Some(SslVersion::TLS1_2)).unwrap();
            b.set_cipher_list("ECDHE-RSA-AES256-SHA@SECLEVEL=0")
                .unwrap();
            b.cert_store_mut()
                .add_cert(FIXTURE.ca.cert().clone())
                .unwrap();
            let ssl = b.build().configure().unwrap().into_ssl(host).unwrap();
            let tcp = TcpStream::connect(addr).await.unwrap();
            let mut tls = tokio_openssl::SslStream::new(ssl, tcp).unwrap();
            Pin::new(&mut tls).connect().await.map(drop)
        };
        connect("127.0.0.1").await.expect("IP SAN must verify");
        assert!(
            connect("127.0.0.2").await.is_err(),
            "another IP must fail verification"
        );
        server.await.unwrap();
    }

    /// A self-signed CA with an EC key, so verifying an RSA-signed cert
    /// against it errors (wrong public key type) instead of returning false.
    fn ec_ca() -> Ca {
        let key = p256_key();
        let mut name = X509NameBuilder::new().unwrap();
        name.append_entry_by_nid(Nid::COMMONNAME, "Controlz EC test CA")
            .unwrap();
        let name = name.build();
        let mut b = builder(&name, &name, &key, 30).unwrap();
        b.append_extension(
            BasicConstraints::new()
                .critical()
                .ca()
                .pathlen(0)
                .build()
                .unwrap(),
        )
        .unwrap();
        b.sign(&key, MessageDigest::sha256()).unwrap();
        Ca(Identity {
            cert: b.build(),
            key,
        })
    }

    #[test]
    fn verify_error_means_reissue() {
        let ca = ec_ca();
        let ca_key = ca.cert().public_key().unwrap();
        assert!(
            FIXTURE.server.cert().verify(&ca_key).is_err(),
            "precondition: verify() must error, not return false"
        );
        assert!(!server_is_current(&FIXTURE.server, &ca, HOSTNAME).unwrap());

        // On disk: the EC CA next to the fixture's RSA-signed server cert.
        let dir = std::env::temp_dir().join(format!("controlz-pki-ecca-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        FIXTURE.save(&dir).unwrap();
        write_pair(&dir, CA_CERT, CA_KEY, ca.identity()).unwrap();
        let loaded = Pki::load_or_create(&dir, HOSTNAME).unwrap();
        assert!(loaded.server.cert().verify(&ca_key).unwrap(), "re-issued");
        assert!(server_is_current(&loaded.server, &loaded.ca, HOSTNAME).unwrap());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn saved_files_are_private_and_reload() {
        let dir = std::env::temp_dir().join(format!("controlz-pki-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        FIXTURE.save(&dir).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(CA_KEY)), 0o600);
        assert_eq!(mode(&dir.join(CLIENT_CA_KEY)), 0o600);
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
        assert!(
            loaded
                .client_ca
                .identity()
                .key()
                .public_eq(FIXTURE.client_ca.identity().key())
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
        assert!(loaded.server.has_san(HOSTNAME));
        assert!(server_is_current(&loaded.server, &loaded.ca, HOSTNAME).unwrap());
        assert!(!server_is_current(&FIXTURE.server, &loaded.ca, HOSTNAME).unwrap());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_client_ca_is_generated() {
        let dir = std::env::temp_dir().join(format!("controlz-pki-newcca-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        FIXTURE.save(&dir).unwrap();
        fs::remove_file(dir.join(CLIENT_CA_CERT)).unwrap();
        fs::remove_file(dir.join(CLIENT_CA_KEY)).unwrap();

        let loaded = Pki::load_or_create(&dir, HOSTNAME).unwrap();
        let mode = fs::metadata(dir.join(CLIENT_CA_KEY))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        assert!(
            !loaded
                .client_ca
                .identity()
                .key()
                .public_eq(FIXTURE.client_ca.identity().key())
        );
        assert!(
            !loaded
                .client_ca
                .identity()
                .key()
                .public_eq(loaded.ca.identity().key())
        );
        assert_eq!(loaded.client_ca.cert().pathlen(), Some(0));
        assert_eq!(
            loaded.server.cert().to_der().unwrap(),
            FIXTURE.server.cert().to_der().unwrap(),
            "the server CA and certificate are unaffected"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn module_pins_are_private_hashed_by_uuid_and_survive_reload() {
        const UUID: &str = "00000000-fake-0000-0000-000000000000";
        let dir = std::env::temp_dir().join(format!("controlz-pins-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let pins = ModulePins::new(&dir);
        assert_eq!(pins.get(UUID).unwrap(), None);
        let pin = "AB".repeat(32);
        assert_eq!(pins.pin_first(UUID, &pin).unwrap(), "ab".repeat(32));

        let files: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        let expected = format!(
            "module-pin-{}.txt",
            hex::encode(&openssl::sha::sha256(UUID.as_bytes())[..8])
        );
        assert_eq!(files, vec![expected.clone()], "one file, no temp left");
        assert!(!expected.contains("fake"), "no uuid in the file name");
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(&expected)), 0o600);

        // A new store on the same directory (a restart) sees the pin.
        let reloaded = ModulePins::new(&dir);
        assert_eq!(reloaded.get(UUID).unwrap(), Some("ab".repeat(32)));
        assert_eq!(reloaded.get("11111111-fake").unwrap(), None);

        assert!(pins.pin_first(UUID, "not-hex").is_err());
        fs::write(dir.join(&expected), "garbage\n").unwrap();
        assert!(matches!(
            reloaded.get(UUID),
            Err(ControlError::Invalid { .. })
        ));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_existing_module_pin_is_never_overwritten() {
        const UUID: &str = "00000000-fake-0000-0000-000000000000";
        let dir = std::env::temp_dir().join(format!("controlz-pins-excl-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let first = "ab".repeat(32);
        let other = "cd".repeat(32);
        // Two stores on one directory stand in for two processes.
        let a = ModulePins::new(&dir);
        let b = ModulePins::new(&dir);
        assert_eq!(a.pin_first(UUID, &first).unwrap(), first);
        assert_eq!(
            b.pin_first(UUID, &other).unwrap(),
            first,
            "existing pin wins"
        );
        assert_eq!(a.pin_first(UUID, &first).unwrap(), first, "same key: no-op");
        assert_eq!(b.get(UUID).unwrap(), Some(first.clone()));
        let file = a.path(UUID);
        assert_eq!(fs::read_to_string(&file).unwrap(), format!("{first}\n"));
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(names, vec![file.clone()], "no temp file left");

        // A file that is not a pin is not replaced either: re-pinning fails.
        fs::write(&file, "garbage\n").unwrap();
        assert!(matches!(
            b.pin_first(UUID, &other),
            Err(ControlError::Invalid { .. })
        ));
        assert_eq!(fs::read_to_string(&file).unwrap(), "garbage\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A leftover temp file is an error that names it, not "already
    /// pinned"; it is left alone and no pin is written.
    #[test]
    fn a_leftover_temp_file_is_an_error_not_an_existing_pin() {
        const UUID: &str = "00000000-fake-0000-0000-000000000000";
        let dir = std::env::temp_dir().join(format!("controlz-pins-tmp-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let pins = ModulePins::new(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut tmp = pins.path(UUID).into_os_string();
        tmp.push(format!(".{}.tmp", std::process::id()));
        let tmp = PathBuf::from(tmp);
        fs::write(&tmp, "leftover\n").unwrap();

        let err = pins.pin_first(UUID, &"ab".repeat(32)).unwrap_err();
        assert!(matches!(err, ControlError::Config { .. }), "{err:?}");
        assert!(err.to_string().contains(&*tmp.to_string_lossy()), "{err}");
        assert_eq!(pins.get(UUID).unwrap(), None);
        assert_eq!(fs::read_to_string(&tmp).unwrap(), "leftover\n");
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

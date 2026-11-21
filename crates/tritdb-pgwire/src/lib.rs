#![forbid(unsafe_code)]
//! Minimal PostgreSQL wire protocol (v3) server for trit-db.
//!
//! Supported: startup handshake, `SSLRequest` rejection, simple `Query`
//! messages, `Terminate`, and `Sync`. Extended query protocol
//! (`Parse`/`Bind`/`Execute`) is not implemented; clients must use the simple
//! query protocol (most `psql`/driver defaults do).
//!
//! Connections are handled on their own threads and share one [`SqlEngine`]
//! behind a mutex, so statements from different clients serialize on the
//! engine.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use md5::{Digest as _, Md5};

use tritdb_exec::{DataType, Value};
use tritdb_sql::{parse_script, QueryResult, SqlEngine, Statement};

const PROTOCOL_V3: i32 = 196_608;
/// SCRAM-SHA-256 iteration count used for new challenges.
const SCRAM_ITERATIONS: u32 = 4_096;
const SSL_REQUEST: i32 = 80_877_103;

/// Server errors.
#[derive(Debug)]
pub enum PgError {
    /// Network or file I/O error.
    Io(std::io::Error),
    /// Malformed client message.
    Protocol(String),
    /// SQL engine failed to open.
    Sql(String),
}

impl fmt::Display for PgError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PgError::Io(error) => write!(formatter, "pgwire io error: {error}"),
            PgError::Protocol(reason) => write!(formatter, "pgwire protocol error: {reason}"),
            PgError::Sql(reason) => write!(formatter, "pgwire sql error: {reason}"),
        }
    }
}

impl std::error::Error for PgError {}

impl From<std::io::Error> for PgError {
    fn from(error: std::io::Error) -> Self {
        PgError::Io(error)
    }
}

/// Byte stream the protocol runs on: a plain `TcpStream` or a TLS stream.
trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

/// A persisted SCRAM-SHA-256 verifier, in PostgreSQL's on-disk form:
/// `SCRAM-SHA-256$<iterations>:<base64 salt>$<base64 stored key>:<base64 server key>`.
///
/// Holding only the verifier means the server never needs the cleartext
/// password at runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScramVerifier {
    /// PBKDF2 iteration count.
    pub iterations: u32,
    /// PBKDF2 salt.
    pub salt: Vec<u8>,
    /// `SHA256(HMAC(SaltedPassword, "Client Key"))`.
    pub stored_key: [u8; 32],
    /// `HMAC(SaltedPassword, "Server Key")`.
    pub server_key: [u8; 32],
}

impl ScramVerifier {
    /// Derives a verifier from a cleartext password with a fresh random salt.
    pub fn from_password(password: &str, iterations: u32) -> Result<Self, PgError> {
        if iterations == 0 {
            return Err(PgError::Protocol(
                "SCRAM iteration count must be greater than 0".to_string(),
            ));
        }
        let salt = random_bytes::<16>()?;
        let salted = pbkdf2_sha256(password.as_bytes(), &salt, iterations);
        let client_key = tritdb_cas::hmac_sha256(&salted, b"Client Key");
        Ok(Self {
            iterations,
            salt: salt.to_vec(),
            stored_key: tritdb_cas::sha256(&client_key),
            server_key: tritdb_cas::hmac_sha256(&salted, b"Server Key"),
        })
    }

    /// Serializes the verifier in PostgreSQL's `SCRAM-SHA-256$...` form.
    pub fn encode(&self) -> String {
        format!(
            "SCRAM-SHA-256${}:{}${}:{}",
            self.iterations,
            BASE64.encode(&self.salt),
            BASE64.encode(self.stored_key),
            BASE64.encode(self.server_key)
        )
    }

    /// Parses a `SCRAM-SHA-256$...` verifier (one trailing newline is allowed).
    pub fn parse(text: &str) -> Result<Self, PgError> {
        let rest = text.trim().strip_prefix("SCRAM-SHA-256$").ok_or_else(|| {
            PgError::Protocol("verifier must start with SCRAM-SHA-256$".to_string())
        })?;
        let (params, keys) = rest
            .split_once('$')
            .ok_or_else(|| PgError::Protocol("verifier is missing its key section".to_string()))?;
        let (iterations, salt) = params
            .split_once(':')
            .ok_or_else(|| PgError::Protocol("verifier is missing iterations:salt".to_string()))?;
        let (stored, server) = keys.split_once(':').ok_or_else(|| {
            PgError::Protocol("verifier is missing stored:server keys".to_string())
        })?;
        let iterations: u32 = iterations
            .parse()
            .map_err(|_| PgError::Protocol("bad verifier iteration count".to_string()))?;
        if iterations == 0 {
            return Err(PgError::Protocol(
                "verifier iteration count must be greater than 0".to_string(),
            ));
        }
        let decode_key = |value: &str, label: &str| -> Result<[u8; 32], PgError> {
            let bytes = BASE64
                .decode(value)
                .map_err(|error| PgError::Protocol(format!("bad {label}: {error}")))?;
            bytes
                .try_into()
                .map_err(|_| PgError::Protocol(format!("{label} must be 32 bytes")))
        };
        Ok(Self {
            iterations,
            salt: BASE64
                .decode(salt)
                .map_err(|error| PgError::Protocol(format!("bad verifier salt: {error}")))?,
            stored_key: decode_key(stored, "verifier stored key")?,
            server_key: decode_key(server, "verifier server key")?,
        })
    }
}

/// How the server authenticates clients.
#[derive(Clone, Debug, Default)]
pub enum AuthMode {
    /// Accept any client without a password (development default).
    #[default]
    Trust,
    /// Ask for a cleartext password and compare it with this value.
    Password(String),
    /// `AuthenticationMD5Password`: `md5(md5(password + user) + salt)`.
    Md5(String),
    /// SASL `SCRAM-SHA-256` derived from a cleartext password at connect time.
    ScramSha256(String),
    /// SASL `SCRAM-SHA-256` backed by a persisted verifier (no cleartext
    /// password needed at runtime).
    ScramSha256Verifier(ScramVerifier),
}

/// Server-side options. `Default` is trust auth with plain text connections,
/// matching the original behaviour.
#[derive(Clone, Debug, Default)]
pub struct ServerOptions {
    /// Authentication mode.
    pub auth: AuthMode,
    /// PEM certificate chain; requires `tls_key` to enable TLS.
    pub tls_cert: Option<PathBuf>,
    /// PEM private key; requires `tls_cert` to enable TLS.
    pub tls_key: Option<PathBuf>,
    /// PEM CA bundle used to verify client certificates (mTLS).
    pub tls_client_ca: Option<PathBuf>,
    /// File of revoked client-certificate SHA-256 fingerprints, one hex digest
    /// per line (`#` starts a comment).
    pub tls_client_revoked: Option<PathBuf>,
    /// Reject clients that do not request TLS.
    pub require_tls: bool,
    /// Permit cleartext password authentication over a plaintext connection.
    pub allow_plaintext_password: bool,
}

impl ServerOptions {
    /// True when both halves of a TLS key pair are configured.
    pub fn tls_enabled(&self) -> bool {
        self.tls_cert.is_some() && self.tls_key.is_some()
    }
}

/// What the client sent before the startup packet was accepted.
enum Startup {
    /// The client disconnected cleanly.
    Closed,
    /// A plain protocol-v3 startup packet (with its `user` parameter).
    Plain { user: Option<String> },
    /// The client asked to negotiate TLS first.
    SslRequest,
}

/// Extracts the `user` parameter from a startup packet body.
fn parse_startup_user(body: &[u8]) -> Option<String> {
    let mut fields = body[4..].split(|byte| *byte == 0);
    while let Some(key) = fields.next() {
        let value = fields.next()?;
        if key.is_empty() {
            return None;
        }
        if key == b"user" {
            return Some(String::from_utf8_lossy(value).to_string());
        }
    }
    None
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right) {
        diff |= a ^ b;
    }
    diff == 0
}

fn random_bytes<const N: usize>() -> Result<[u8; N], PgError> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes)
        .map_err(|error| PgError::Protocol(format!("random bytes: {error}")))?;
    Ok(bytes)
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut out, "{byte:02x}").expect("write hex");
    }
    out
}

fn md5_hex(bytes: &[u8]) -> String {
    hex_lower(&Md5::digest(bytes))
}

/// PBKDF2-HMAC-SHA256 with a 32-byte output (a single block).
fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut block = Vec::with_capacity(salt.len() + 4);
    block.extend_from_slice(salt);
    block.extend_from_slice(&1u32.to_be_bytes());
    let mut u = tritdb_cas::hmac_sha256(password, &block);
    let mut acc = u;
    for _ in 1..iterations {
        u = tritdb_cas::hmac_sha256(password, &u);
        for (left, right) in acc.iter_mut().zip(u) {
            *left ^= right;
        }
    }
    acc
}

fn read_password_message(stream: &mut dyn ReadWrite) -> Result<Vec<u8>, PgError> {
    let Some((kind, payload)) = read_message(stream)? else {
        return Err(PgError::Protocol(
            "client closed the connection during authentication".to_string(),
        ));
    };
    if kind != b'p' {
        write_error(stream, "expected a PasswordMessage")?;
        return Err(PgError::Protocol(format!(
            "expected PasswordMessage, got {}",
            kind as char
        )));
    }
    Ok(payload)
}

fn auth_failed(stream: &mut dyn ReadWrite) -> Result<(), PgError> {
    write_error(stream, "password authentication failed")?;
    Err(PgError::Protocol(
        "password authentication failed".to_string(),
    ))
}

fn authenticate_md5(
    stream: &mut dyn ReadWrite,
    password: &str,
    user: Option<&str>,
) -> Result<(), PgError> {
    let Some(user) = user else {
        write_error(stream, "MD5 authentication requires a username")?;
        return Err(PgError::Protocol("missing startup username".to_string()));
    };
    let salt = random_bytes::<4>()?;
    let mut request = Vec::with_capacity(8);
    request.extend_from_slice(&5i32.to_be_bytes());
    request.extend_from_slice(&salt);
    write_message(stream, b'R', &request)?;

    let payload = read_password_message(stream)?;
    let supplied = String::from_utf8_lossy(&payload)
        .trim_end_matches('\0')
        .to_string();
    let inner = md5_hex(format!("{password}{user}").as_bytes());
    let mut outer_input = inner.into_bytes();
    outer_input.extend_from_slice(&salt);
    let expected = md5_hex(&outer_input);
    if constant_time_eq(supplied.as_bytes(), expected.as_bytes()) {
        write_message(stream, b'R', &0i32.to_be_bytes())
    } else {
        auth_failed(stream)
    }
}

fn authenticate_scram(stream: &mut dyn ReadWrite, verifier: &ScramVerifier) -> Result<(), PgError> {
    // AuthenticationSASL (10) advertising SCRAM-SHA-256.
    let mut payload = Vec::new();
    payload.extend_from_slice(&10i32.to_be_bytes());
    payload.extend_from_slice(b"SCRAM-SHA-256\0\0");
    write_message(stream, b'R', &payload)?;

    let initial = read_password_message(stream)?;
    let (mechanism, cursor) = read_cstring(&initial, 0)?;
    if mechanism != "SCRAM-SHA-256" {
        write_error(stream, &format!("unsupported SASL mechanism {mechanism}"))?;
        return Err(PgError::Protocol(format!(
            "unsupported SASL mechanism {mechanism}"
        )));
    }
    let length = read_i32(&initial, cursor)?;
    if length < 0 {
        write_error(stream, "bad SASL initial response")?;
        return Err(PgError::Protocol("bad SASL initial response".to_string()));
    }
    let start = cursor + 4;
    let end = start + length as usize;
    let client_first = initial
        .get(start..end)
        .ok_or_else(|| PgError::Protocol("truncated SASL initial response".to_string()))?;
    let client_first = String::from_utf8_lossy(client_first).to_string();

    // Skip the GS2 header ("n,," / "y,,"); the rest is the bare message.
    let bare_start = client_first
        .find(",,")
        .map(|index| index + 2)
        .ok_or_else(|| PgError::Protocol("malformed SCRAM client-first-message".to_string()))?;
    let client_first_bare = client_first[bare_start..].to_string();
    let client_nonce = client_first_bare
        .split(',')
        .find_map(|part| part.strip_prefix("r="))
        .ok_or_else(|| PgError::Protocol("missing SCRAM client nonce".to_string()))?
        .to_string();

    let nonce = format!("{client_nonce}{}", hex_lower(&random_bytes::<18>()?));
    let server_first = format!(
        "r={nonce},s={},i={}",
        BASE64.encode(&verifier.salt),
        verifier.iterations
    );
    let mut payload = Vec::new();
    payload.extend_from_slice(&11i32.to_be_bytes());
    payload.extend_from_slice(server_first.as_bytes());
    write_message(stream, b'R', &payload)?;

    let final_message = read_password_message(stream)?;
    let client_final = String::from_utf8_lossy(&final_message)
        .trim_end_matches('\0')
        .to_string();
    let Some(proof_at) = client_final.rfind(",p=") else {
        write_error(stream, "malformed SCRAM client-final-message")?;
        return Err(PgError::Protocol(
            "malformed SCRAM client-final-message".to_string(),
        ));
    };
    let client_final_without_proof = client_final[..proof_at].to_string();
    let proof = BASE64
        .decode(&client_final[proof_at + 3..])
        .map_err(|error| PgError::Protocol(format!("bad SCRAM proof: {error}")))?;
    if proof.len() != 32 {
        write_error(stream, "bad SCRAM proof length")?;
        return Err(PgError::Protocol("bad SCRAM proof length".to_string()));
    }

    let auth_message = format!("{client_first_bare},{server_first},{client_final_without_proof}");
    let client_signature = tritdb_cas::hmac_sha256(&verifier.stored_key, auth_message.as_bytes());
    let mut recovered = [0u8; 32];
    for (index, value) in recovered.iter_mut().enumerate() {
        *value = proof[index] ^ client_signature[index];
    }
    if !constant_time_eq(&tritdb_cas::sha256(&recovered), &verifier.stored_key) {
        return auth_failed(stream);
    }
    let server_signature = tritdb_cas::hmac_sha256(&verifier.server_key, auth_message.as_bytes());
    let mut payload = Vec::new();
    payload.extend_from_slice(&12i32.to_be_bytes());
    payload.extend_from_slice(format!("v={}", BASE64.encode(server_signature)).as_bytes());
    write_message(stream, b'R', &payload)?;
    write_message(stream, b'R', &0i32.to_be_bytes())
}

/// Runs the authentication exchange configured on the server.
fn authenticate(
    stream: &mut dyn ReadWrite,
    auth: &AuthMode,
    user: Option<&str>,
) -> Result<(), PgError> {
    match auth {
        AuthMode::Trust => write_message(stream, b'R', &0i32.to_be_bytes()),
        AuthMode::Password(expected) => {
            write_message(stream, b'R', &3i32.to_be_bytes())?;
            let payload = read_password_message(stream)?;
            let supplied = String::from_utf8_lossy(&payload)
                .trim_end_matches('\0')
                .to_string();
            if constant_time_eq(supplied.as_bytes(), expected.as_bytes()) {
                write_message(stream, b'R', &0i32.to_be_bytes())
            } else {
                auth_failed(stream)
            }
        }
        AuthMode::Md5(password) => authenticate_md5(stream, password, user),
        AuthMode::ScramSha256(password) => {
            let verifier = ScramVerifier::from_password(password, SCRAM_ITERATIONS)?;
            authenticate_scram(stream, &verifier)
        }
        AuthMode::ScramSha256Verifier(verifier) => authenticate_scram(stream, verifier),
    }
}

/// Wraps a client-certificate verifier and rejects certificates whose SHA-256
/// fingerprint appears in a revocation list.
#[derive(Debug)]
struct RevocationVerifier {
    inner: Arc<dyn rustls::server::danger::ClientCertVerifier>,
    revoked: Vec<[u8; 32]>,
}

impl rustls::server::danger::ClientCertVerifier for RevocationVerifier {
    fn offer_client_auth(&self) -> bool {
        self.inner.offer_client_auth()
    }

    fn client_auth_mandatory(&self) -> bool {
        self.inner.client_auth_mandatory()
    }

    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        self.inner.root_hint_subjects()
    }

    fn verify_client_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        intermediates: &[rustls::pki_types::CertificateDer<'_>],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::server::danger::ClientCertVerified, rustls::Error> {
        let fingerprint = tritdb_cas::sha256(end_entity.as_ref());
        if self
            .revoked
            .iter()
            .any(|revoked| constant_time_eq(revoked, &fingerprint))
        {
            return Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::Revoked,
            ));
        }
        self.inner
            .verify_client_cert(end_entity, intermediates, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// Parses a revocation list: one SHA-256 hex fingerprint per line,
/// `#` starts a comment, `:`/`-`/spaces inside a fingerprint are ignored.
fn load_revoked(path: &Path) -> Result<Vec<[u8; 32]>, PgError> {
    let text = std::fs::read_to_string(path)?;
    let mut revoked = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let hex: String = line
            .chars()
            .filter(|character| !matches!(character, ':' | '-' | ' '))
            .collect();
        if hex.len() != 64 {
            return Err(PgError::Protocol(format!(
                "revoked fingerprint must be 64 hex characters: {line}"
            )));
        }
        let mut bytes = [0u8; 32];
        for (index, chunk) in hex.as_bytes().chunks(2).enumerate() {
            let pair = std::str::from_utf8(chunk)
                .map_err(|_| PgError::Protocol(format!("bad hex in {line}")))?;
            bytes[index] = u8::from_str_radix(pair, 16)
                .map_err(|_| PgError::Protocol(format!("bad hex in {line}")))?;
        }
        revoked.push(bytes);
    }
    Ok(revoked)
}

fn load_tls_config(options: &ServerOptions) -> Result<Arc<rustls::ServerConfig>, PgError> {
    let (Some(cert_path), Some(key_path)) = (&options.tls_cert, &options.tls_key) else {
        return Err(PgError::Protocol(
            "TLS needs both tls_cert and tls_key".to_string(),
        ));
    };
    let cert_bytes = std::fs::read(cert_path)?;
    let key_bytes = std::fs::read(key_path)?;
    let certs = rustls_pemfile::certs(&mut cert_bytes.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| PgError::Protocol(format!("bad certificate PEM: {error}")))?;
    if certs.is_empty() {
        return Err(PgError::Protocol(
            "certificate file contains no certificates".to_string(),
        ));
    }
    let key = rustls_pemfile::private_key(&mut key_bytes.as_slice())
        .map_err(|error| PgError::Protocol(format!("bad private key PEM: {error}")))?
        .ok_or_else(|| PgError::Protocol("private key file contains no key".to_string()))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| PgError::Protocol(format!("TLS versions: {error}")))?;
    let builder = match &options.tls_client_ca {
        Some(ca_path) => {
            let ca_bytes = std::fs::read(ca_path)?;
            let ca_certs = rustls_pemfile::certs(&mut ca_bytes.as_slice())
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| PgError::Protocol(format!("bad client CA PEM: {error}")))?;
            let mut roots = rustls::RootCertStore::empty();
            for cert in ca_certs {
                roots
                    .add(cert)
                    .map_err(|error| PgError::Protocol(format!("client CA: {error}")))?;
            }
            let verifier_provider = Arc::new(rustls::crypto::ring::default_provider());
            let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
                Arc::new(roots),
                verifier_provider,
            )
            .build()
            .map_err(|error| PgError::Protocol(format!("client verifier: {error}")))?;
            let verifier: Arc<dyn rustls::server::danger::ClientCertVerifier> =
                match &options.tls_client_revoked {
                    Some(path) => Arc::new(RevocationVerifier {
                        inner: verifier,
                        revoked: load_revoked(path)?,
                    }),
                    None => verifier,
                };
            builder.with_client_cert_verifier(verifier)
        }
        None => builder.with_no_client_auth(),
    };
    let config = builder
        .with_single_cert(certs, key)
        .map_err(|error| PgError::Protocol(format!("TLS certificate: {error}")))?;
    Ok(Arc::new(config))
}

/// Reloads the TLS configuration when its files change on disk.
struct TlsReloader {
    options: ServerOptions,
    cached: Mutex<CachedTls>,
}

struct CachedTls {
    fingerprint: [u8; 32],
    config: Arc<rustls::ServerConfig>,
}

impl TlsReloader {
    fn new(options: &ServerOptions) -> Result<Self, PgError> {
        let fingerprint = tls_fingerprint(options)?;
        let config = load_tls_config(options)?;
        Ok(Self {
            options: options.clone(),
            cached: Mutex::new(CachedTls {
                fingerprint,
                config,
            }),
        })
    }

    /// Returns the current configuration, reloading it when the PEM files
    /// changed. A reload failure keeps the previous certificate.
    fn snapshot(&self) -> Result<Arc<rustls::ServerConfig>, PgError> {
        let fingerprint = tls_fingerprint(&self.options)?;
        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if cached.fingerprint != fingerprint {
            match load_tls_config(&self.options) {
                Ok(config) => {
                    cached.config = config;
                    cached.fingerprint = fingerprint;
                    eprintln!("pgwire: reloaded TLS certificate");
                }
                Err(error) => eprintln!(
                    "pgwire: TLS reload failed, keeping the previous certificate: {error}"
                ),
            }
        }
        Ok(Arc::clone(&cached.config))
    }
}

fn tls_fingerprint(options: &ServerOptions) -> Result<[u8; 32], PgError> {
    let mut input = Vec::new();
    for path in [
        options.tls_cert.as_ref(),
        options.tls_key.as_ref(),
        options.tls_client_ca.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        input.extend_from_slice(&std::fs::read(path)?);
    }
    Ok(tritdb_cas::sha256(&input))
}

/// Serves connections until the listener is closed or fails.
///
/// Connections are accepted and handled serially; each one opens its own
/// [`SqlEngine`] on `db`.
pub fn serve(db: PathBuf, listener: TcpListener) -> Result<(), PgError> {
    serve_with_options(db, listener, Arc::new(ServerOptions::default()))
}

/// Serves connections with explicit authentication and TLS options.
pub fn serve_with_options(
    db: PathBuf,
    listener: TcpListener,
    options: Arc<ServerOptions>,
) -> Result<(), PgError> {
    if matches!(options.auth, AuthMode::Password(_))
        && !options.tls_enabled()
        && !options.allow_plaintext_password
    {
        return Err(PgError::Protocol(
            "cleartext password authentication over a plaintext connection needs \
             allow_plaintext_password (or enable TLS)"
                .to_string(),
        ));
    }
    let tls = if options.tls_enabled() {
        Some(Arc::new(TlsReloader::new(&options)?))
    } else {
        None
    };
    let engine = Arc::new(Mutex::new(
        SqlEngine::open(&db).map_err(|error| PgError::Sql(error.to_string()))?,
    ));
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("pgwire: accept failed: {error}");
                continue;
            }
        };
        let engine = Arc::clone(&engine);
        let options = Arc::clone(&options);
        let tls = tls.clone();
        std::thread::spawn(move || {
            if let Err(error) = handle_connection(engine, stream, options, tls) {
                eprintln!("pgwire: {error}");
            }
        });
    }
    Ok(())
}

fn lock_engine(engine: &Arc<Mutex<SqlEngine>>) -> std::sync::MutexGuard<'_, SqlEngine> {
    engine
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn handle_connection(
    engine: Arc<Mutex<SqlEngine>>,
    mut stream: TcpStream,
    options: Arc<ServerOptions>,
    tls: Option<Arc<TlsReloader>>,
) -> Result<(), PgError> {
    // Postgres-style clients send one small message per round trip; leaving
    // Nagle on makes every response wait for the peer's delayed ACK (~40 ms on
    // Linux). Disable it per connection.
    stream.set_nodelay(true)?;

    let user: Option<String>;
    let mut conn: Box<dyn ReadWrite> = match read_startup(&mut stream)? {
        Startup::Closed => return Ok(()),
        Startup::Plain { user: name } => {
            user = name;
            if options.require_tls {
                write_error(&mut stream, "TLS is required by this server")?;
                return Err(PgError::Protocol("TLS is required".to_string()));
            }
            Box::new(stream)
        }
        Startup::SslRequest => match tls.as_ref() {
            Some(reloader) => {
                stream.write_all(b"S")?;
                let config = reloader.snapshot()?;
                let server = rustls::ServerConnection::new(config)
                    .map_err(|error| PgError::Protocol(format!("TLS handshake: {error}")))?;
                let mut tls_stream = rustls::StreamOwned::new(server, stream);
                match read_startup(&mut tls_stream)? {
                    Startup::Plain { user: name } => user = name,
                    Startup::Closed => return Ok(()),
                    Startup::SslRequest => {
                        return Err(PgError::Protocol(
                            "client repeated SSLRequest inside TLS".to_string(),
                        ))
                    }
                }
                Box::new(tls_stream)
            }
            None => {
                stream.write_all(b"N")?;
                match read_startup(&mut stream)? {
                    Startup::Plain { user: name } => {
                        user = name;
                        if options.require_tls {
                            write_error(&mut stream, "TLS is required by this server")?;
                            return Err(PgError::Protocol("TLS is required".to_string()));
                        }
                        Box::new(stream)
                    }
                    Startup::Closed => return Ok(()),
                    Startup::SslRequest => {
                        return Err(PgError::Protocol(
                            "client repeated SSLRequest after it was refused".to_string(),
                        ))
                    }
                }
            }
        },
    };

    let stream: &mut dyn ReadWrite = conn.as_mut();
    authenticate(stream, &options.auth, user.as_deref())?;
    // A few ParameterStatus values that clients expect.
    write_parameter(stream, "server_version", "14.0")?;
    write_parameter(stream, "server_encoding", "UTF8")?;
    write_parameter(stream, "client_encoding", "UTF8")?;
    write_parameter(stream, "DateStyle", "ISO, MDY")?;
    // BackendKeyData
    let mut key = Vec::with_capacity(8);
    key.extend_from_slice(&1i32.to_be_bytes());
    key.extend_from_slice(&0i32.to_be_bytes());
    write_message(stream, b'K', &key)?;
    write_message(stream, b'Z', b"I")?;

    let mut prepared: BTreeMap<String, Prepared> = BTreeMap::new();
    let mut portals: BTreeMap<String, Bound> = BTreeMap::new();
    let mut tx_status = TxStatus::Idle;

    loop {
        let Some((kind, payload)) = read_message(stream)? else {
            return Ok(());
        };
        match kind {
            b'Q' => {
                let sql = String::from_utf8_lossy(&payload)
                    .trim_end_matches('\0')
                    .to_string();
                let mut guard = lock_engine(&engine);
                handle_query(stream, &mut guard, &sql, &mut tx_status)?;
            }
            b'S' => write_message(stream, b'Z', &[tx_status.byte()])?,
            b'X' => return Ok(()),
            b'P' => parse_prepared(stream, &mut prepared, &payload)?,
            b'B' => bind_portal(stream, &prepared, &mut portals, &payload)?,
            b'D' => {
                let mut guard = lock_engine(&engine);
                describe(stream, &mut guard, &prepared, &portals, &payload)?;
            }
            b'E' => {
                let mut guard = lock_engine(&engine);
                execute_portal(stream, &mut guard, &portals, &payload, &mut tx_status)?;
            }
            b'C' => close_portal(stream, &mut prepared, &mut portals, &payload)?,
            b'H' => {}
            other => {
                write_error(
                    stream,
                    &format!("unsupported message type {}", other as char),
                )?;
                write_message(stream, b'Z', &[tx_status.byte()])?;
            }
        }
    }
}

/// Reads the startup message (handling `SSLRequest`), returning `false` when
/// the client disconnected cleanly.
fn read_startup(stream: &mut dyn ReadWrite) -> Result<Startup, PgError> {
    let mut length = [0u8; 4];
    if stream.read_exact(&mut length).is_err() {
        return Ok(Startup::Closed);
    }
    let length = i32::from_be_bytes(length);
    if !(8..=1_000_000).contains(&length) {
        return Err(PgError::Protocol(format!("bad startup length {length}")));
    }
    let mut body = vec![0u8; (length - 4) as usize];
    stream.read_exact(&mut body)?;
    let protocol = i32::from_be_bytes([body[0], body[1], body[2], body[3]]);
    if protocol == SSL_REQUEST {
        return Ok(Startup::SslRequest);
    }
    if protocol != PROTOCOL_V3 {
        return Err(PgError::Protocol(format!(
            "unsupported protocol version {protocol}"
        )));
    }
    // The rest is key\0value\0...\0; keep the `user` parameter.
    let user = parse_startup_user(&body);
    Ok(Startup::Plain { user })
}

/// Reads one backend message, returning `None` on clean EOF.
fn read_message(stream: &mut dyn ReadWrite) -> Result<Option<(u8, Vec<u8>)>, PgError> {
    let mut kind = [0u8; 1];
    if stream.read_exact(&mut kind).is_err() {
        return Ok(None);
    }
    let mut length = [0u8; 4];
    stream.read_exact(&mut length)?;
    let length = i32::from_be_bytes(length);
    if !(4..=16_000_000).contains(&length) {
        return Err(PgError::Protocol(format!("bad message length {length}")));
    }
    let mut payload = vec![0u8; (length - 4) as usize];
    stream.read_exact(&mut payload)?;
    Ok(Some((kind[0], payload)))
}

fn write_message(stream: &mut dyn ReadWrite, kind: u8, payload: &[u8]) -> Result<(), PgError> {
    let length = (payload.len() as i32) + 4;
    stream.write_all(&[kind])?;
    stream.write_all(&length.to_be_bytes())?;
    stream.write_all(payload)?;
    Ok(())
}

fn write_parameter(stream: &mut dyn ReadWrite, key: &str, value: &str) -> Result<(), PgError> {
    let mut payload = Vec::with_capacity(key.len() + value.len() + 2);
    payload.extend_from_slice(key.as_bytes());
    payload.push(0);
    payload.extend_from_slice(value.as_bytes());
    payload.push(0);
    write_message(stream, b'S', &payload)
}

/// Connection-level transaction state used by `ReadyForQuery`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TxStatus {
    Idle,
    InTransaction,
    Failed,
}

impl TxStatus {
    fn byte(self) -> u8 {
        match self {
            TxStatus::Idle => b'I',
            TxStatus::InTransaction => b'T',
            TxStatus::Failed => b'E',
        }
    }
}

enum TxCommand {
    Begin,
    Commit,
    Rollback,
}

fn transaction_command(sql: &str) -> Option<TxCommand> {
    let normalized = sql.trim().trim_end_matches(';').trim().to_ascii_lowercase();
    match normalized.as_str() {
        "begin" | "start transaction" => Some(TxCommand::Begin),
        "commit" | "end" => Some(TxCommand::Commit),
        "rollback" | "abort" => Some(TxCommand::Rollback),
        _ => None,
    }
}

enum CopyDirection {
    In,
    Out,
}

struct CopyPlan {
    table: String,
    columns: Option<Vec<String>>,
    direction: CopyDirection,
}

/// Recognises `COPY <table> [(<cols>)] FROM STDIN` / `TO STDOUT`.
fn parse_copy(sql: &str) -> Option<CopyPlan> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    if trimmed.len() < 5 || !trimmed[..4].eq_ignore_ascii_case("copy") {
        return None;
    }
    let rest = trimmed[4..].trim_start();
    let upper = rest.to_ascii_uppercase();
    let (head, direction) = if let Some(at) = upper.rfind(" FROM STDIN") {
        (&rest[..at], CopyDirection::In)
    } else {
        let at = upper.rfind(" TO STDOUT")?;
        (&rest[..at], CopyDirection::Out)
    };
    let head = head.trim();
    let (table, columns) = match head.find('(') {
        Some(open) => {
            let close = head.rfind(')')?;
            let table = head[..open].trim().trim_matches('"').to_ascii_lowercase();
            let columns: Vec<String> = head[open + 1..close]
                .split(',')
                .map(|column| column.trim().trim_matches('"').to_ascii_lowercase())
                .filter(|column| !column.is_empty())
                .collect();
            (table, Some(columns))
        }
        None => (head.trim().trim_matches('"').to_ascii_lowercase(), None),
    };
    if table.is_empty() {
        return None;
    }
    Some(CopyPlan {
        table,
        columns,
        direction,
    })
}

fn copy_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Unescapes one COPY text-format value (`\\`, `\t`, `\n`, `\r`).
fn copy_unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn copy_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out
}

/// Builds a SQL literal for one COPY value, using the target column's type.
fn copy_literal(value: &str, data_type: DataType) -> String {
    if value == "\\N" {
        return "NULL".to_string();
    }
    let value = copy_unescape(value);
    match data_type {
        DataType::Int | DataType::Lukasiewicz | DataType::Nat => {
            if value.parse::<i64>().is_ok() || value.parse::<f64>().is_ok() {
                value
            } else {
                copy_quote(&value)
            }
        }
        DataType::Bool | DataType::Logic | DataType::Belnap => {
            let lowered = value.to_ascii_lowercase();
            match lowered.as_str() {
                "t" | "true" => "TRUE".to_string(),
                "f" | "false" => "FALSE".to_string(),
                "both" | "neither" => value.to_ascii_uppercase(),
                _ => copy_quote(&value),
            }
        }
        DataType::Text => copy_quote(&value),
    }
}

fn copy_failed(stream: &mut dyn ReadWrite, message: &str) -> Result<bool, PgError> {
    write_error(stream, message)?;
    Ok(false)
}

fn handle_copy(
    stream: &mut dyn ReadWrite,
    engine: &mut SqlEngine,
    plan: &CopyPlan,
) -> Result<bool, PgError> {
    let Some(schema) = engine.table_schema(&plan.table) else {
        return copy_failed(stream, &format!("unknown table {}", plan.table));
    };
    let columns = match &plan.columns {
        Some(columns) => columns.clone(),
        None => schema.iter().map(|(name, _)| name.clone()).collect(),
    };
    let mut types = Vec::with_capacity(columns.len());
    for column in &columns {
        let Some((_, data_type)) = schema.iter().find(|(name, _)| name == column) else {
            return copy_failed(stream, &format!("unknown column {column}"));
        };
        types.push(*data_type);
    }
    match plan.direction {
        CopyDirection::In => copy_in(stream, engine, &plan.table, &columns, &types),
        CopyDirection::Out => copy_out(stream, engine, &plan.table, &columns),
    }
}

fn copy_in(
    stream: &mut dyn ReadWrite,
    engine: &mut SqlEngine,
    table: &str,
    columns: &[String],
    types: &[DataType],
) -> Result<bool, PgError> {
    let mut payload = Vec::with_capacity(3 + columns.len() * 2);
    payload.push(0); // text format
    payload.extend_from_slice(&(columns.len() as i16).to_be_bytes());
    for _ in columns {
        payload.extend_from_slice(&0i16.to_be_bytes());
    }
    write_message(stream, b'G', &payload)?;

    let mut inserted = 0usize;
    let mut batch = String::new();
    let mut batched = 0usize;
    loop {
        let Some((kind, payload)) = read_message(stream)? else {
            return Err(PgError::Protocol(
                "client disconnected during COPY".to_string(),
            ));
        };
        match kind {
            b'd' => {
                let line = String::from_utf8_lossy(&payload);
                let line = line.trim_end_matches(['\r', '\n']);
                let values: Vec<&str> = line.split('\t').collect();
                if values.len() != columns.len() {
                    return copy_failed(
                        stream,
                        &format!(
                            "COPY expected {} columns, got {}",
                            columns.len(),
                            values.len()
                        ),
                    );
                }
                if batched == 0 {
                    batch.push_str(&format!(
                        "INSERT INTO {table} ({}) VALUES ",
                        columns.join(", ")
                    ));
                } else {
                    batch.push(',');
                }
                batch.push('(');
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        batch.push(',');
                    }
                    batch.push_str(&copy_literal(value, types[index]));
                }
                batch.push(')');
                batched += 1;
                if batched >= 1024 {
                    batch.push(';');
                    if let Err(error) = engine.execute_script(&batch) {
                        return copy_failed(stream, &error.to_string());
                    }
                    inserted += batched;
                    batch.clear();
                    batched = 0;
                }
            }
            b'c' => break,
            b'f' => {
                let message = String::from_utf8_lossy(&payload)
                    .trim_end_matches('\0')
                    .to_string();
                return copy_failed(stream, &message);
            }
            other => {
                return Err(PgError::Protocol(format!(
                    "unexpected message {} during COPY",
                    other as char
                )));
            }
        }
    }
    if batched > 0 {
        batch.push(';');
        if let Err(error) = engine.execute_script(&batch) {
            return copy_failed(stream, &error.to_string());
        }
        inserted += batched;
    }
    let tag = format!("COPY {inserted}\0");
    write_message(stream, b'C', tag.as_bytes())?;
    Ok(true)
}

fn copy_out(
    stream: &mut dyn ReadWrite,
    engine: &mut SqlEngine,
    table: &str,
    columns: &[String],
) -> Result<bool, PgError> {
    let sql = format!("SELECT {} FROM {table};", columns.join(", "));
    let results = match engine.execute_script(&sql) {
        Ok(results) => results,
        Err(error) => return copy_failed(stream, &error.to_string()),
    };
    let mut payload = Vec::with_capacity(3 + columns.len() * 2);
    payload.push(0);
    payload.extend_from_slice(&(columns.len() as i16).to_be_bytes());
    for _ in columns {
        payload.extend_from_slice(&0i16.to_be_bytes());
    }
    write_message(stream, b'H', &payload)?;
    let mut sent = 0usize;
    for result in results {
        if let QueryResult::Rows { rows, .. } = result {
            for row in rows {
                let mut line = String::new();
                for (index, value) in row.iter().enumerate() {
                    if index > 0 {
                        line.push('\t');
                    }
                    if value.is_null() {
                        line.push_str("\\N");
                    } else {
                        line.push_str(&copy_escape(&value_text(value)));
                    }
                }
                line.push('\n');
                write_message(stream, b'd', line.as_bytes())?;
                sent += 1;
            }
        }
    }
    write_message(stream, b'c', &[])?;
    let tag = format!("COPY {sent}\0");
    write_message(stream, b'C', tag.as_bytes())?;
    Ok(true)
}

fn handle_query(
    stream: &mut dyn ReadWrite,
    engine: &mut SqlEngine,
    sql: &str,
    tx_status: &mut TxStatus,
) -> Result<(), PgError> {
    if sql.trim().is_empty() {
        write_message(stream, b'I', &[])?;
        write_message(stream, b'Z', &[tx_status.byte()])?;
        return Ok(());
    }
    if let Some(command) = transaction_command(sql) {
        match command {
            TxCommand::Begin => {
                if *tx_status == TxStatus::Failed {
                    write_error(
                        stream,
                        "current transaction is aborted, commands ignored until end of transaction block",
                    )?;
                } else {
                    *tx_status = TxStatus::InTransaction;
                    write_message(stream, b'C', b"BEGIN\0")?;
                }
            }
            TxCommand::Commit => {
                if *tx_status == TxStatus::Failed {
                    *tx_status = TxStatus::Idle;
                    write_message(stream, b'C', b"ROLLBACK\0")?;
                } else {
                    *tx_status = TxStatus::Idle;
                    write_message(stream, b'C', b"COMMIT\0")?;
                }
            }
            TxCommand::Rollback => {
                *tx_status = TxStatus::Idle;
                write_message(stream, b'C', b"ROLLBACK\0")?;
            }
        }
        write_message(stream, b'Z', &[tx_status.byte()])?;
        return Ok(());
    }
    if *tx_status == TxStatus::Failed {
        write_error(
            stream,
            "current transaction is aborted, commands ignored until end of transaction block",
        )?;
        write_message(stream, b'Z', &[tx_status.byte()])?;
        return Ok(());
    }
    let success = match parse_copy(sql) {
        Some(plan) => handle_copy(stream, engine, &plan)?,
        None => execute_and_write(stream, engine, sql, &[])?,
    };
    if !success && *tx_status == TxStatus::InTransaction {
        *tx_status = TxStatus::Failed;
    }
    write_message(stream, b'Z', &[tx_status.byte()])?;
    Ok(())
}

fn execute_and_write(
    stream: &mut dyn ReadWrite,
    engine: &mut SqlEngine,
    sql: &str,
    result_formats: &[i16],
) -> Result<bool, PgError> {
    match engine.execute_script(sql) {
        Ok(results) => {
            for result in results {
                match result {
                    QueryResult::Rows { columns, rows } => {
                        let oids = column_oids(&rows, columns.len());
                        write_row_description(stream, &columns, &oids, result_formats)?;
                        for row in &rows {
                            write_data_row(stream, row, &oids, result_formats)?;
                        }
                        let tag = format!("SELECT {}\0", rows.len());
                        write_message(stream, b'C', tag.as_bytes())?;
                    }
                    QueryResult::Message(message) => {
                        let tag = format!("{}\0", command_tag(&message));
                        write_message(stream, b'C', tag.as_bytes())?;
                    }
                }
            }
            Ok(true)
        }
        Err(error) => {
            write_error(stream, &error.to_string())?;
            Ok(false)
        }
    }
}

struct Prepared {
    sql: String,
    param_types: Vec<i32>,
}

struct Bound {
    sql: String,
    result_formats: Vec<i16>,
}

fn read_cstring(payload: &[u8], offset: usize) -> Result<(String, usize), PgError> {
    let end = payload[offset..]
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| PgError::Protocol("unterminated string".to_string()))?
        + offset;
    Ok((
        String::from_utf8_lossy(&payload[offset..end]).to_string(),
        end + 1,
    ))
}

fn read_i16(payload: &[u8], offset: usize) -> Result<i16, PgError> {
    let bytes: [u8; 2] = payload
        .get(offset..offset + 2)
        .ok_or_else(|| PgError::Protocol("truncated int16".to_string()))?
        .try_into()
        .map_err(|_| PgError::Protocol("truncated int16".to_string()))?;
    Ok(i16::from_be_bytes(bytes))
}

fn read_i32(payload: &[u8], offset: usize) -> Result<i32, PgError> {
    let bytes: [u8; 4] = payload
        .get(offset..offset + 4)
        .ok_or_else(|| PgError::Protocol("truncated int32".to_string()))?
        .try_into()
        .map_err(|_| PgError::Protocol("truncated int32".to_string()))?;
    Ok(i32::from_be_bytes(bytes))
}

fn parse_prepared(
    stream: &mut dyn ReadWrite,
    prepared: &mut BTreeMap<String, Prepared>,
    payload: &[u8],
) -> Result<(), PgError> {
    let (name, cursor) = read_cstring(payload, 0)?;
    let (sql, cursor) = read_cstring(payload, cursor)?;
    let count = read_i16(payload, cursor)?;
    let mut cursor = cursor + 2;
    let mut param_types = Vec::with_capacity(count.max(0) as usize);
    for _ in 0..count.max(0) {
        param_types.push(read_i32(payload, cursor)?);
        cursor += 4;
    }
    prepared.insert(name, Prepared { sql, param_types });
    write_message(stream, b'1', &[])
}

fn bind_portal(
    stream: &mut dyn ReadWrite,
    prepared: &BTreeMap<String, Prepared>,
    portals: &mut BTreeMap<String, Bound>,
    payload: &[u8],
) -> Result<(), PgError> {
    let (portal, cursor) = read_cstring(payload, 0)?;
    let (statement, mut cursor) = read_cstring(payload, cursor)?;
    let format_count = read_i16(payload, cursor)?;
    cursor += 2 + format_count.max(0) as usize * 2;
    let param_count = read_i16(payload, cursor)?;
    cursor += 2;
    let prepared = prepared
        .get(&statement)
        .ok_or_else(|| PgError::Protocol(format!("unknown prepared statement {statement:?}")))?;
    let mut params = Vec::with_capacity(param_count.max(0) as usize);
    for _ in 0..param_count.max(0) {
        let length = read_i32(payload, cursor)?;
        cursor += 4;
        if length < 0 {
            params.push(None);
        } else {
            let length = length as usize;
            let value = payload
                .get(cursor..cursor + length)
                .ok_or_else(|| PgError::Protocol("truncated parameter".to_string()))?;
            params.push(Some(String::from_utf8_lossy(value).to_string()));
            cursor += length;
        }
    }
    let sql = substitute_params(&prepared.sql, &prepared.param_types, &params)?;
    let result_format_count = read_i16(payload, cursor)?;
    cursor += 2;
    let mut result_formats = Vec::with_capacity(result_format_count.max(0) as usize);
    for _ in 0..result_format_count.max(0) {
        result_formats.push(read_i16(payload, cursor)?);
        cursor += 2;
    }
    portals.insert(
        portal,
        Bound {
            sql,
            result_formats,
        },
    );
    write_message(stream, b'2', &[])
}

fn describe(
    stream: &mut dyn ReadWrite,
    engine: &mut SqlEngine,
    prepared: &BTreeMap<String, Prepared>,
    portals: &BTreeMap<String, Bound>,
    payload: &[u8],
) -> Result<(), PgError> {
    let kind = payload
        .first()
        .copied()
        .ok_or_else(|| PgError::Protocol("empty Describe".to_string()))?;
    let (name, _) = read_cstring(payload, 1)?;
    match kind {
        b'S' => {
            let prepared = prepared
                .get(&name)
                .ok_or_else(|| PgError::Protocol(format!("unknown prepared statement {name:?}")))?;
            let mut p = Vec::new();
            p.extend_from_slice(&(prepared.param_types.len() as i16).to_be_bytes());
            for oid in &prepared.param_types {
                p.extend_from_slice(&oid.to_be_bytes());
            }
            write_message(stream, b't', &p)?;
            describe_result(stream, engine, &prepared.sql, &prepared.param_types, &[])?;
        }
        b'P' => {
            let bound = portals
                .get(&name)
                .ok_or_else(|| PgError::Protocol(format!("unknown portal {name:?}")))?;
            describe_result(stream, engine, &bound.sql, &[], &bound.result_formats)?;
        }
        other => {
            return Err(PgError::Protocol(format!(
                "bad Describe kind {}",
                other as char
            )));
        }
    }
    Ok(())
}

/// Writes a `RowDescription` when the statement is a `SELECT`; otherwise
/// `NoData`.
fn describe_result(
    stream: &mut dyn ReadWrite,
    engine: &mut SqlEngine,
    sql: &str,
    param_types: &[i32],
    result_formats: &[i16],
) -> Result<(), PgError> {
    match describe_columns(engine, sql, param_types) {
        Ok(Some(meta)) => write_row_description(stream, &meta.columns, &meta.oids, result_formats),
        Ok(None) => write_message(stream, b'n', &[]),
        Err(error) => write_error(stream, &error.to_string()),
    }
}

/// Result column names plus the PostgreSQL type OIDs inferred from a sample
/// row.
struct ResultMeta {
    columns: Vec<String>,
    oids: Vec<i32>,
}

/// Prepares a query with `LIMIT 1` to learn the result column names and types.
/// Unbound parameters are substituted with `NULL`.
fn describe_columns(
    engine: &mut SqlEngine,
    sql: &str,
    param_types: &[i32],
) -> Result<Option<ResultMeta>, PgError> {
    let placeholders = vec![None; param_types.len()];
    let sql = substitute_params(sql, param_types, &placeholders)?;
    let statements = parse_script(&sql).map_err(|error| PgError::Sql(error.to_string()))?;
    let Some(Statement::Select(select)) = statements.first() else {
        return Ok(None);
    };
    let mut limited = select.clone();
    limited.limit = Some(1);
    match engine
        .execute_statement(&Statement::Select(limited))
        .map_err(|error| PgError::Sql(error.to_string()))?
    {
        QueryResult::Rows { columns, rows } => {
            let oids = column_oids(&rows, columns.len());
            Ok(Some(ResultMeta { columns, oids }))
        }
        QueryResult::Message(_) => Ok(None),
    }
}

/// Infers PostgreSQL type OIDs from the result rows: `INT` → `int8`(20),
/// `BOOL` → `bool`(16), everything else (including `NULL`-only columns) →
/// `text`(25).
fn column_oids(rows: &[Vec<Value>], column_count: usize) -> Vec<i32> {
    let mut oids = vec![25; column_count];
    let mut known = vec![false; column_count];
    for row in rows {
        for (index, value) in row.iter().enumerate() {
            if known[index] || value.is_null() {
                continue;
            }
            oids[index] = match value {
                Value::Int(_) => 20,
                Value::Bool(_) => 16,
                _ => 25,
            };
            known[index] = true;
        }
    }
    oids
}

fn execute_portal(
    stream: &mut dyn ReadWrite,
    engine: &mut SqlEngine,
    portals: &BTreeMap<String, Bound>,
    payload: &[u8],
    tx_status: &mut TxStatus,
) -> Result<(), PgError> {
    let (portal, cursor) = read_cstring(payload, 0)?;
    let _max_rows = read_i32(payload, cursor)?;
    let bound = portals
        .get(&portal)
        .ok_or_else(|| PgError::Protocol(format!("unknown portal {portal:?}")))?;
    if *tx_status == TxStatus::Failed {
        write_error(
            stream,
            "current transaction is aborted, commands ignored until end of transaction block",
        )?;
        return Ok(());
    }
    let success = execute_and_write(stream, engine, &bound.sql, &bound.result_formats)?;
    if !success && *tx_status == TxStatus::InTransaction {
        *tx_status = TxStatus::Failed;
    }
    Ok(())
}

fn close_portal(
    stream: &mut dyn ReadWrite,
    prepared: &mut BTreeMap<String, Prepared>,
    portals: &mut BTreeMap<String, Bound>,
    payload: &[u8],
) -> Result<(), PgError> {
    let kind = payload
        .first()
        .copied()
        .ok_or_else(|| PgError::Protocol("empty Close".to_string()))?;
    let (name, _) = read_cstring(payload, 1)?;
    match kind {
        b'S' => {
            prepared.remove(&name);
        }
        b'P' => {
            portals.remove(&name);
        }
        other => {
            return Err(PgError::Protocol(format!(
                "bad Close kind {}",
                other as char
            )));
        }
    }
    write_message(stream, b'3', &[])
}

fn substitute_params(
    sql: &str,
    param_types: &[i32],
    params: &[Option<String>],
) -> Result<String, PgError> {
    let mut output = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    let mut in_string = false;
    while let Some(character) = chars.next() {
        if character == '\'' {
            in_string = !in_string;
            output.push(character);
            continue;
        }
        if character == '$' && !in_string {
            let mut digits = String::new();
            while let Some(next) = chars.peek() {
                if next.is_ascii_digit() {
                    digits.push(*next);
                    chars.next();
                } else {
                    break;
                }
            }
            if digits.is_empty() {
                output.push('$');
                continue;
            }
            let index: usize = digits
                .parse()
                .map_err(|_| PgError::Protocol("bad parameter index".to_string()))?;
            if index == 0 || index > params.len() {
                return Err(PgError::Protocol(format!(
                    "parameter ${index} out of range ({} supplied)",
                    params.len()
                )));
            }
            let oid = param_types.get(index - 1).copied().unwrap_or(0);
            output.push_str(&param_literal(&params[index - 1], oid));
            continue;
        }
        output.push(character);
    }
    Ok(output)
}

fn param_literal(value: &Option<String>, oid: i32) -> String {
    let Some(text) = value else {
        return "NULL".to_string();
    };
    match oid {
        20 | 21 | 23 => text.clone(),
        16 => {
            if text == "t" || text.eq_ignore_ascii_case("true") {
                "TRUE".to_string()
            } else {
                "FALSE".to_string()
            }
        }
        25 | 1043 => format!("'{}'", text.replace('\'', "''")),
        _ => {
            if text.parse::<i64>().is_ok() {
                text.clone()
            } else if text.eq_ignore_ascii_case("true") || text == "t" {
                "TRUE".to_string()
            } else if text.eq_ignore_ascii_case("false") || text == "f" {
                "FALSE".to_string()
            } else {
                format!("'{}'", text.replace('\'', "''"))
            }
        }
    }
}

fn write_row_description(
    stream: &mut dyn ReadWrite,
    columns: &[String],
    oids: &[i32],
    formats: &[i16],
) -> Result<(), PgError> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&(columns.len() as i16).to_be_bytes());
    for (index, name) in columns.iter().enumerate() {
        let oid = oids.get(index).copied().unwrap_or(25);
        let size: i16 = match oid {
            20 => 8, // int8
            16 => 1, // bool
            _ => -1, // text
        };
        payload.extend_from_slice(name.as_bytes());
        payload.push(0);
        payload.extend_from_slice(&0i32.to_be_bytes()); // table oid
        payload.extend_from_slice(&0i16.to_be_bytes()); // attribute number
        payload.extend_from_slice(&oid.to_be_bytes());
        payload.extend_from_slice(&size.to_be_bytes());
        payload.extend_from_slice(&(-1i32).to_be_bytes()); // type modifier
        payload.extend_from_slice(&format_for_column(formats, index).to_be_bytes());
    }
    write_message(stream, b'T', &payload)
}

/// PostgreSQL result format rule: zero codes means text, one code applies to
/// every column, otherwise each column has its own code.
fn format_for_column(formats: &[i16], index: usize) -> i16 {
    match formats.len() {
        0 => 0,
        1 => formats[0],
        _ => formats.get(index).copied().unwrap_or(0),
    }
}

fn value_binary(value: &Value, oid: i32) -> Vec<u8> {
    match (oid, value) {
        (20, Value::Int(number)) => number.to_be_bytes().to_vec(),
        (16, Value::Bool(flag)) => vec![u8::from(*flag)],
        _ => value_text(value).into_bytes(),
    }
}

fn write_data_row(
    stream: &mut dyn ReadWrite,
    row: &[Value],
    oids: &[i32],
    formats: &[i16],
) -> Result<(), PgError> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&(row.len() as i16).to_be_bytes());
    for (index, value) in row.iter().enumerate() {
        if value.is_null() {
            payload.extend_from_slice(&(-1i32).to_be_bytes());
            continue;
        }
        if format_for_column(formats, index) == 1 {
            let bytes = value_binary(value, oids.get(index).copied().unwrap_or(25));
            payload.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
            payload.extend_from_slice(&bytes);
        } else {
            let text = value_text(value);
            payload.extend_from_slice(&(text.len() as i32).to_be_bytes());
            payload.extend_from_slice(text.as_bytes());
        }
    }
    write_message(stream, b'D', &payload)
}

fn value_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Int(value) => value.to_string(),
        Value::Text(value) => value.clone(),
        Value::Bool(value) => {
            if *value {
                "t".to_string()
            } else {
                "f".to_string()
            }
        }
        Value::Logic(value) => value.as_str().to_string(),
        Value::Lukasiewicz(value) => format!("{:.3}", f64::from(*value) / 1000.0),
        Value::Nat(value) => value.to_string(),
    }
}

fn write_error(stream: &mut dyn ReadWrite, message: &str) -> Result<(), PgError> {
    let mut payload = Vec::new();
    payload.push(b'S');
    payload.extend_from_slice(b"ERROR\0");
    payload.push(b'C');
    payload.extend_from_slice(b"XX000\0");
    payload.push(b'M');
    payload.extend_from_slice(message.as_bytes());
    payload.push(0);
    payload.push(0);
    write_message(stream, b'E', &payload)
}

/// Maps an engine acknowledgement to a PostgreSQL command tag.
fn command_tag(message: &str) -> String {
    if message.starts_with("inserted ") {
        let count = message
            .split_whitespace()
            .nth(1)
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        return format!("INSERT 0 {count}");
    }
    if message.starts_with("updated ") {
        let count = message
            .split_whitespace()
            .nth(1)
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        return format!("UPDATE {count}");
    }
    if message.starts_with("deleted ") {
        let count = message
            .split_whitespace()
            .nth(1)
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        return format!("DELETE {count}");
    }
    if message.starts_with("created table") {
        return "CREATE TABLE".to_string();
    }
    if message.starts_with("dropped table") {
        return "DROP TABLE".to_string();
    }
    if message.starts_with("added column") || message.starts_with("dropped column") {
        return "ALTER TABLE".to_string();
    }
    if message.starts_with("renamed") || message.starts_with("altered column") {
        return "ALTER TABLE".to_string();
    }
    if message.starts_with("analyzed") {
        return "ANALYZE".to_string();
    }
    "OK".to_string()
}

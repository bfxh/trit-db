//! Protocol-level robustness tests for the minimal PostgreSQL wire server:
//! startup edge cases, unknown message types, malformed lengths, Close/Bind on
//! unknown names, repeated Sync and transaction-state transitions.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use md5::{Digest as _, Md5};
use tritdb_testkit::TempDir;

fn start_server(name: &str) -> (TempDir, TcpStream) {
    start_server_with(name, tritdb_pgwire::ServerOptions::default())
}

fn start_server_with(name: &str, options: tritdb_pgwire::ServerOptions) -> (TempDir, TcpStream) {
    let dir = TempDir::new(name);
    let db = dir.join("protocol.db");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let db_path = db.clone();
    let options = std::sync::Arc::new(options);
    thread::spawn(move || {
        let _ = tritdb_pgwire::serve_with_options(db_path, listener, options);
    });
    let stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    (dir, stream)
}

fn write_startup<S: Write>(stream: &mut S) {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608i32.to_be_bytes());
    body.push(0);
    let mut message = Vec::new();
    message.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    message.extend_from_slice(&body);
    stream.write_all(&message).expect("startup");
}

fn write_message<S: Write>(stream: &mut S, kind: u8, payload: &[u8]) {
    let mut message = vec![kind];
    message.extend_from_slice(&((payload.len() + 4) as i32).to_be_bytes());
    message.extend_from_slice(payload);
    stream.write_all(&message).expect("message");
}

fn query<S: Write>(stream: &mut S, sql: &str) {
    let mut payload = sql.as_bytes().to_vec();
    payload.push(0);
    write_message(stream, b'Q', &payload);
}

fn read_message<S: Read>(stream: &mut S) -> Option<(u8, Vec<u8>)> {
    let mut kind = [0u8; 1];
    if stream.read_exact(&mut kind).is_err() {
        return None;
    }
    let mut length = [0u8; 4];
    stream.read_exact(&mut length).expect("message length");
    let length = i32::from_be_bytes(length) as usize - 4;
    let mut payload = vec![0u8; length];
    stream.read_exact(&mut payload).expect("message payload");
    Some((kind[0], payload))
}

fn read_until_ready<S: Read>(stream: &mut S) -> Vec<(u8, Vec<u8>)> {
    let mut messages = Vec::new();
    loop {
        let Some((kind, payload)) = read_message(stream) else {
            return messages;
        };
        messages.push((kind, payload));
        if kind == b'Z' {
            return messages;
        }
    }
}

fn parse_row(payload: &[u8]) -> Vec<Option<String>> {
    let count = i16::from_be_bytes([payload[0], payload[1]]) as usize;
    let mut cursor = 2;
    let mut values = Vec::new();
    for _ in 0..count {
        let length = i32::from_be_bytes([
            payload[cursor],
            payload[cursor + 1],
            payload[cursor + 2],
            payload[cursor + 3],
        ]);
        cursor += 4;
        if length < 0 {
            values.push(None);
        } else {
            let length = length as usize;
            values.push(Some(
                String::from_utf8_lossy(&payload[cursor..cursor + length]).to_string(),
            ));
            cursor += length;
        }
    }
    values
}

fn ready_status(messages: &[(u8, Vec<u8>)]) -> Option<u8> {
    messages
        .iter()
        .find(|(kind, _)| *kind == b'Z')
        .map(|(_, payload)| payload[0])
}

fn has_error(messages: &[(u8, Vec<u8>)]) -> bool {
    messages.iter().any(|(kind, _)| *kind == b'E')
}

/// Returns true when the server has closed the connection.
fn connection_closed<S: Read>(stream: &mut S) -> bool {
    let mut buffer = [0u8; 16];
    match stream.read(&mut buffer) {
        Ok(0) => true,
        Ok(_) => false,
        Err(_) => true,
    }
}

#[test]
fn ssl_request_is_refused_then_plain_startup_succeeds() {
    let (_dir, mut stream) = start_server("pgproto-ssl");
    // SSLRequest: length 8, code 80877103.
    let mut ssl = Vec::new();
    ssl.extend_from_slice(&8i32.to_be_bytes());
    ssl.extend_from_slice(&80_877_103i32.to_be_bytes());
    stream.write_all(&ssl).expect("ssl request");
    let mut reply = [0u8; 1];
    stream.read_exact(&mut reply).expect("ssl reply");
    assert_eq!(reply[0], b'N', "server must refuse TLS");

    write_startup(&mut stream);
    let messages = read_until_ready(&mut stream);
    assert!(messages.iter().any(|(kind, _)| *kind == b'R'));
    assert!(messages.iter().any(|(kind, _)| *kind == b'K'));
    assert_eq!(ready_status(&messages), Some(b'I'));
}

#[test]
fn empty_query_returns_empty_query_response() {
    let (_dir, mut stream) = start_server("pgproto-empty");
    write_startup(&mut stream);
    let _ = read_until_ready(&mut stream);

    query(&mut stream, "");
    let messages = read_until_ready(&mut stream);
    assert!(
        messages.iter().any(|(kind, _)| *kind == b'I'),
        "missing EmptyQueryResponse: {messages:?}"
    );
    assert_eq!(ready_status(&messages), Some(b'I'));
}

#[test]
fn unknown_message_type_reports_error_and_keeps_connection() {
    let (_dir, mut stream) = start_server("pgproto-unknown");
    write_startup(&mut stream);
    let _ = read_until_ready(&mut stream);

    write_message(&mut stream, b'?', b"");
    let messages = read_until_ready(&mut stream);
    assert!(has_error(&messages), "unknown message must report an error");
    assert_eq!(ready_status(&messages), Some(b'I'));

    // The connection still works afterwards.
    query(&mut stream, "SELECT 1;");
    let messages = read_until_ready(&mut stream);
    assert!(messages.iter().any(|(kind, _)| *kind == b'D'));
    assert_eq!(ready_status(&messages), Some(b'I'));
}

#[test]
fn flush_is_ignored_but_sync_still_replies() {
    let (_dir, mut stream) = start_server("pgproto-flush");
    write_startup(&mut stream);
    let _ = read_until_ready(&mut stream);

    write_message(&mut stream, b'H', b"");
    write_message(&mut stream, b'S', b"");
    let messages = read_until_ready(&mut stream);
    assert_eq!(messages.len(), 1, "Flush must not produce a message");
    assert_eq!(ready_status(&messages), Some(b'I'));
}

#[test]
fn repeated_sync_returns_ready_for_query_each_time() {
    let (_dir, mut stream) = start_server("pgproto-sync");
    write_startup(&mut stream);
    let _ = read_until_ready(&mut stream);

    write_message(&mut stream, b'S', b"");
    assert_eq!(ready_status(&read_until_ready(&mut stream)), Some(b'I'));
    write_message(&mut stream, b'S', b"");
    assert_eq!(ready_status(&read_until_ready(&mut stream)), Some(b'I'));
}

#[test]
fn close_unknown_statement_and_portal_completes() {
    let (_dir, mut stream) = start_server("pgproto-close");
    write_startup(&mut stream);
    let _ = read_until_ready(&mut stream);

    // Close('S', "") and Close('P', "missing") are both accepted.
    let mut payload = vec![b'S'];
    payload.extend_from_slice(b"missing\0");
    write_message(&mut stream, b'C', &payload);
    let messages = read_until_ready_with_sync(&mut stream);
    assert!(messages.iter().any(|(kind, _)| *kind == b'3'));

    let mut payload = vec![b'P'];
    payload.extend_from_slice(b"missing\0");
    write_message(&mut stream, b'C', &payload);
    let messages = read_until_ready_with_sync(&mut stream);
    assert!(messages.iter().any(|(kind, _)| *kind == b'3'));
}

fn read_until_ready_with_sync<S: Read + Write>(stream: &mut S) -> Vec<(u8, Vec<u8>)> {
    write_message(stream, b'S', b"");
    read_until_ready(stream)
}

#[test]
fn bind_unknown_statement_closes_connection() {
    let (_dir, mut stream) = start_server("pgproto-bind");
    write_startup(&mut stream);
    let _ = read_until_ready(&mut stream);

    // Bind with a portal name and an unknown statement name.
    let mut payload = Vec::new();
    payload.extend_from_slice(b"p1\0missing\0");
    payload.extend_from_slice(&0i16.to_be_bytes()); // format count
    payload.extend_from_slice(&0i16.to_be_bytes()); // param count
    write_message(&mut stream, b'B', &payload);
    assert!(
        connection_closed(&mut stream),
        "binding an unknown statement must close the connection"
    );
}

#[test]
fn invalid_message_length_closes_connection() {
    let (_dir, mut stream) = start_server("pgproto-badlen");
    write_startup(&mut stream);
    let _ = read_until_ready(&mut stream);

    // A message length below the 4-byte minimum is rejected.
    stream.write_all(b"Q\x00\x00\x00\x01").expect("bad length");
    assert!(
        connection_closed(&mut stream),
        "bad message length must close the connection"
    );
}

#[test]
fn unsupported_protocol_version_closes_connection() {
    let (_dir, mut stream) = start_server("pgproto-version");
    let mut body = Vec::new();
    body.extend_from_slice(&123_456i32.to_be_bytes());
    body.push(0);
    let mut message = Vec::new();
    message.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    message.extend_from_slice(&body);
    stream.write_all(&message).expect("bad startup");
    assert!(
        connection_closed(&mut stream),
        "unsupported protocol version must close the connection"
    );
}

#[test]
fn failed_transaction_commit_reports_rollback() {
    let (_dir, mut stream) = start_server("pgproto-txfail");
    write_startup(&mut stream);
    let _ = read_until_ready(&mut stream);

    query(&mut stream, "CREATE TABLE t (id INT NOT NULL);");
    let _ = read_until_ready(&mut stream);

    query(&mut stream, "BEGIN;");
    assert_eq!(ready_status(&read_until_ready(&mut stream)), Some(b'T'));

    query(&mut stream, "SELECT * FROM missing;");
    let messages = read_until_ready(&mut stream);
    assert!(has_error(&messages));
    assert_eq!(ready_status(&messages), Some(b'E'));

    // Statements stay rejected in the aborted transaction.
    query(&mut stream, "SELECT id FROM t;");
    let messages = read_until_ready(&mut stream);
    assert!(has_error(&messages));
    assert_eq!(ready_status(&messages), Some(b'E'));

    // COMMIT on a failed transaction rolls back.
    query(&mut stream, "COMMIT;");
    let messages = read_until_ready(&mut stream);
    assert!(messages
        .iter()
        .any(|(kind, payload)| *kind == b'C' && payload == b"ROLLBACK\0"));
    assert_eq!(ready_status(&messages), Some(b'I'));
}

#[test]
fn syntax_error_reports_error_then_ready() {
    let (_dir, mut stream) = start_server("pgproto-syntax");
    write_startup(&mut stream);
    let _ = read_until_ready(&mut stream);

    query(&mut stream, "SELECT FROM;");
    let messages = read_until_ready(&mut stream);
    assert!(has_error(&messages));
    assert_eq!(ready_status(&messages), Some(b'I'));

    query(&mut stream, "SELECT 1;");
    let messages = read_until_ready(&mut stream);
    assert!(messages.iter().any(|(kind, _)| *kind == b'D'));
}

#[test]
fn extended_query_substitutes_typed_parameters() {
    let (_dir, mut stream) = start_server("pgproto-params");
    write_startup(&mut stream);
    let _ = read_until_ready(&mut stream);

    query(
        &mut stream,
        "CREATE TABLE t (id INT NOT NULL, name TEXT, flag BOOL);",
    );
    let _ = read_until_ready(&mut stream);

    // Parse("stmt", "INSERT INTO t VALUES ($1, $2, $3)", int8/text/bool).
    let mut payload = Vec::new();
    payload.extend_from_slice(b"stmt\0");
    payload.extend_from_slice(b"INSERT INTO t VALUES ($1, $2, $3);\0");
    payload.extend_from_slice(&3i16.to_be_bytes());
    for oid in [20i32, 25, 16] {
        payload.extend_from_slice(&oid.to_be_bytes());
    }
    write_message(&mut stream, b'P', &payload);

    // Bind three text-format parameters.
    let mut payload = Vec::new();
    payload.extend_from_slice(b"portal\0stmt\0");
    payload.extend_from_slice(&0i16.to_be_bytes());
    payload.extend_from_slice(&3i16.to_be_bytes());
    for value in ["7", "seven", "t"] {
        payload.extend_from_slice(&(value.len() as i32).to_be_bytes());
        payload.extend_from_slice(value.as_bytes());
    }
    payload.extend_from_slice(&0i16.to_be_bytes()); // result format count
    write_message(&mut stream, b'B', &payload);
    let mut execute = b"portal\0".to_vec();
    execute.extend_from_slice(&0i32.to_be_bytes()); // max rows (ignored)
    write_message(&mut stream, b'E', &execute);
    write_message(&mut stream, b'S', b"");
    let messages = read_until_ready(&mut stream);
    assert!(!has_error(&messages), "bind/execute failed: {messages:?}");

    query(&mut stream, "SELECT id, name, flag FROM t;");
    let messages = read_until_ready(&mut stream);
    let row = messages
        .iter()
        .find(|(kind, _)| *kind == b'D')
        .map(|(_, payload)| parse_row(payload))
        .expect("DataRow");
    assert_eq!(
        row,
        vec![
            Some("7".to_string()),
            Some("seven".to_string()),
            Some("t".to_string())
        ]
    );
}

#[test]
fn terminate_closes_connection_cleanly() {
    let (_dir, mut stream) = start_server("pgproto-terminate");
    write_startup(&mut stream);
    let _ = read_until_ready(&mut stream);

    stream.write_all(b"X\x00\x00\x00\x04").expect("terminate");
    assert!(connection_closed(&mut stream));
}

#[test]
fn password_auth_accepts_correct_password() {
    let options = tritdb_pgwire::ServerOptions {
        auth: tritdb_pgwire::AuthMode::Password("s3cret".to_string()),
        allow_plaintext_password: true,
        ..Default::default()
    };
    let (_dir, mut stream) = start_server_with("pgproto-auth-ok", options);
    write_startup(&mut stream);

    let (kind, payload) = read_message(&mut stream).expect("auth request");
    assert_eq!(kind, b'R', "expected AuthenticationCleartextPassword");
    assert_eq!(
        i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]),
        3
    );

    write_message(&mut stream, b'p', b"s3cret\0");
    let messages = read_until_ready(&mut stream);
    assert!(!has_error(&messages), "unexpected error: {messages:?}");
    assert_eq!(ready_status(&messages), Some(b'I'));

    query(&mut stream, "SELECT 1;");
    let messages = read_until_ready(&mut stream);
    assert!(messages.iter().any(|(kind, _)| *kind == b'D'));
}

#[test]
fn password_auth_rejects_wrong_password() {
    let options = tritdb_pgwire::ServerOptions {
        auth: tritdb_pgwire::AuthMode::Password("s3cret".to_string()),
        allow_plaintext_password: true,
        ..Default::default()
    };
    let (_dir, mut stream) = start_server_with("pgproto-auth-bad", options);
    write_startup(&mut stream);

    let (kind, payload) = read_message(&mut stream).expect("auth request");
    assert_eq!(kind, b'R');
    assert_eq!(
        i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]),
        3
    );

    write_message(&mut stream, b'p', b"wrong\0");
    let messages = read_until_ready(&mut stream);
    assert!(has_error(&messages), "wrong password must be rejected");
    assert!(
        connection_closed(&mut stream),
        "the server must close the connection after a failed login"
    );
}

#[test]
fn trust_mode_never_asks_for_a_password() {
    let (_dir, mut stream) = start_server("pgproto-auth-trust");
    write_startup(&mut stream);
    let messages = read_until_ready(&mut stream);
    let auth = messages
        .iter()
        .find(|(kind, _)| *kind == b'R')
        .map(|(_, payload)| i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]))
        .expect("AuthenticationOk");
    assert_eq!(auth, 0, "trust mode must send AuthenticationOk directly");
}

fn tls_client_config(cert: &rcgen::CertifiedKey) -> std::sync::Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(cert.cert.der().clone())
        .expect("trust certificate");
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    std::sync::Arc::new(config)
}

#[test]
fn tls_handshake_and_query() {
    let certified =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("certificate");
    let dir = TempDir::new("pgproto-tls");
    let db = dir.join("tls.db");
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    std::fs::write(&cert_path, certified.cert.pem()).expect("write certificate");
    std::fs::write(&key_path, certified.key_pair.serialize_pem()).expect("write key");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let db_path = db.clone();
    let options = std::sync::Arc::new(tritdb_pgwire::ServerOptions {
        tls_cert: Some(cert_path),
        tls_key: Some(key_path),
        ..Default::default()
    });
    thread::spawn(move || {
        let _ = tritdb_pgwire::serve_with_options(db_path, listener, options);
    });

    let mut tcp = TcpStream::connect(addr).expect("connect");
    tcp.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    // SSLRequest, then the server must answer 'S'.
    let mut ssl = Vec::new();
    ssl.extend_from_slice(&8i32.to_be_bytes());
    ssl.extend_from_slice(&80_877_103i32.to_be_bytes());
    tcp.write_all(&ssl).expect("ssl request");
    let mut reply = [0u8; 1];
    tcp.read_exact(&mut reply).expect("ssl reply");
    assert_eq!(reply[0], b'S', "server must accept TLS");

    let config = tls_client_config(&certified);
    let server_name =
        rustls::pki_types::ServerName::try_from("localhost".to_string()).expect("server name");
    let connection = rustls::ClientConnection::new(config, server_name).expect("client connection");
    let mut tls = rustls::StreamOwned::new(connection, tcp);

    write_startup(&mut tls);
    let messages = read_until_ready(&mut tls);
    assert!(
        !has_error(&messages),
        "startup over TLS failed: {messages:?}"
    );
    assert_eq!(ready_status(&messages), Some(b'I'));

    query(
        &mut tls,
        "CREATE TABLE t (id INT NOT NULL); INSERT INTO t VALUES (1);",
    );
    let _ = read_until_ready(&mut tls);
    query(&mut tls, "SELECT id FROM t;");
    let messages = read_until_ready(&mut tls);
    let row = messages
        .iter()
        .find(|(kind, _)| *kind == b'D')
        .map(|(_, payload)| parse_row(payload))
        .expect("DataRow over TLS");
    assert_eq!(row, vec![Some("1".to_string())]);
}

#[test]
fn require_tls_rejects_plaintext_startup() {
    let certified =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("certificate");
    let dir = TempDir::new("pgproto-tls-required");
    let db = dir.join("tls.db");
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    std::fs::write(&cert_path, certified.cert.pem()).expect("write certificate");
    std::fs::write(&key_path, certified.key_pair.serialize_pem()).expect("write key");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let db_path = db.clone();
    let options = std::sync::Arc::new(tritdb_pgwire::ServerOptions {
        tls_cert: Some(cert_path),
        tls_key: Some(key_path),
        require_tls: true,
        ..Default::default()
    });
    thread::spawn(move || {
        let _ = tritdb_pgwire::serve_with_options(db_path, listener, options);
    });

    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    // Plain startup without SSLRequest must be refused.
    write_startup(&mut stream);
    let messages = read_until_ready(&mut stream);
    assert!(
        has_error(&messages) || messages.is_empty(),
        "plaintext startup must be refused: {messages:?}"
    );
}

#[test]
fn tls_disabled_still_refuses_ssl_request() {
    let (_dir, mut stream) = start_server("pgproto-tls-off");
    let mut ssl = Vec::new();
    ssl.extend_from_slice(&8i32.to_be_bytes());
    ssl.extend_from_slice(&80_877_103i32.to_be_bytes());
    stream.write_all(&ssl).expect("ssl request");
    let mut reply = [0u8; 1];
    stream.read_exact(&mut reply).expect("ssl reply");
    assert_eq!(reply[0], b'N', "without TLS the server must answer 'N'");
}

fn write_startup_with_user<S: Write>(stream: &mut S, user: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608i32.to_be_bytes());
    body.extend_from_slice(b"user\0");
    body.extend_from_slice(user.as_bytes());
    body.push(0);
    body.push(0);
    let mut message = Vec::new();
    message.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    message.extend_from_slice(&body);
    stream.write_all(&message).expect("startup with user");
}

fn md5_hex(bytes: &[u8]) -> String {
    let digest = Md5::digest(bytes);
    let mut out = String::with_capacity(32);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut block = salt.to_vec();
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

fn auth_code(payload: &[u8]) -> i32 {
    i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]])
}

#[test]
fn md5_auth_accepts_correct_password() {
    let options = tritdb_pgwire::ServerOptions {
        auth: tritdb_pgwire::AuthMode::Md5("s3cret".to_string()),
        ..Default::default()
    };
    let (_dir, mut stream) = start_server_with("pgproto-md5-ok", options);
    write_startup_with_user(&mut stream, "tester");

    let (kind, payload) = read_message(&mut stream).expect("MD5 request");
    assert_eq!(kind, b'R');
    assert_eq!(auth_code(&payload), 5);
    let salt = &payload[4..8];

    let inner = md5_hex(b"s3crettester");
    let mut outer_input = inner.into_bytes();
    outer_input.extend_from_slice(salt);
    let response = md5_hex(&outer_input);
    write_message(&mut stream, b'p', format!("{response}\0").as_bytes());

    let messages = read_until_ready(&mut stream);
    assert!(!has_error(&messages), "MD5 login failed: {messages:?}");
    assert_eq!(ready_status(&messages), Some(b'I'));

    query(&mut stream, "SELECT 1;");
    let messages = read_until_ready(&mut stream);
    assert!(messages.iter().any(|(kind, _)| *kind == b'D'));
}

#[test]
fn md5_auth_rejects_wrong_password() {
    let options = tritdb_pgwire::ServerOptions {
        auth: tritdb_pgwire::AuthMode::Md5("s3cret".to_string()),
        ..Default::default()
    };
    let (_dir, mut stream) = start_server_with("pgproto-md5-bad", options);
    write_startup_with_user(&mut stream, "tester");

    let (kind, payload) = read_message(&mut stream).expect("MD5 request");
    assert_eq!(kind, b'R');
    assert_eq!(auth_code(&payload), 5);
    let salt = &payload[4..8];

    let inner = md5_hex(b"wrongtester");
    let mut outer_input = inner.into_bytes();
    outer_input.extend_from_slice(salt);
    let response = md5_hex(&outer_input);
    write_message(&mut stream, b'p', format!("{response}\0").as_bytes());

    let messages = read_until_ready(&mut stream);
    assert!(has_error(&messages), "wrong MD5 password must be rejected");
}

#[test]
fn scram_auth_accepts_correct_password() {
    let options = tritdb_pgwire::ServerOptions {
        auth: tritdb_pgwire::AuthMode::ScramSha256("s3cret".to_string()),
        ..Default::default()
    };
    let (_dir, mut stream) = start_server_with("pgproto-scram-ok", options);
    write_startup_with_user(&mut stream, "tester");

    // AuthenticationSASL (10) + mechanism list.
    let (kind, payload) = read_message(&mut stream).expect("SASL");
    assert_eq!(kind, b'R');
    assert_eq!(auth_code(&payload), 10);
    assert!(payload[4..].starts_with(b"SCRAM-SHA-256\0"));

    let client_nonce = "clientnonce123456";
    let client_first_bare = format!("n=,r={client_nonce}");
    let client_first = format!("n,,{client_first_bare}");
    let mut initial = Vec::new();
    initial.extend_from_slice(b"SCRAM-SHA-256\0");
    initial.extend_from_slice(&(client_first.len() as i32).to_be_bytes());
    initial.extend_from_slice(client_first.as_bytes());
    write_message(&mut stream, b'p', &initial);

    // AuthenticationSASLContinue (11).
    let (kind, payload) = read_message(&mut stream).expect("SASLContinue");
    assert_eq!(kind, b'R');
    assert_eq!(auth_code(&payload), 11);
    let server_first = String::from_utf8_lossy(&payload[4..]).to_string();
    let mut nonce = String::new();
    let mut salt = Vec::new();
    let mut iterations = 0u32;
    for part in server_first.split(',') {
        if let Some(value) = part.strip_prefix("r=") {
            nonce = value.to_string();
        }
        if let Some(value) = part.strip_prefix("s=") {
            salt = BASE64.decode(value).expect("base64 salt");
        }
        if let Some(value) = part.strip_prefix("i=") {
            iterations = value.parse().expect("iterations");
        }
    }
    assert!(nonce.starts_with(client_nonce));
    assert!(!salt.is_empty());
    assert!(iterations > 0);

    let salted = pbkdf2_sha256(b"s3cret", &salt, iterations);
    let client_key = tritdb_cas::hmac_sha256(&salted, b"Client Key");
    let stored_key = tritdb_cas::sha256(&client_key);
    let client_final_without_proof = format!("c=biws,r={nonce}");
    let auth_message = format!("{client_first_bare},{server_first},{client_final_without_proof}");
    let client_signature = tritdb_cas::hmac_sha256(&stored_key, auth_message.as_bytes());
    let mut proof = [0u8; 32];
    for (index, value) in proof.iter_mut().enumerate() {
        *value = client_key[index] ^ client_signature[index];
    }
    let client_final = format!("{client_final_without_proof},p={}", BASE64.encode(proof));
    write_message(&mut stream, b'p', client_final.as_bytes());

    // AuthenticationSASLFinal (12) carries the server signature.
    let (kind, payload) = read_message(&mut stream).expect("SASLFinal");
    assert_eq!(kind, b'R');
    assert_eq!(auth_code(&payload), 12);
    let server_key = tritdb_cas::hmac_sha256(&salted, b"Server Key");
    let server_signature = tritdb_cas::hmac_sha256(&server_key, auth_message.as_bytes());
    assert_eq!(
        String::from_utf8_lossy(&payload[4..]),
        format!("v={}", BASE64.encode(server_signature))
    );

    let messages = read_until_ready(&mut stream);
    assert!(!has_error(&messages), "SCRAM login failed: {messages:?}");
    assert_eq!(ready_status(&messages), Some(b'I'));

    query(&mut stream, "SELECT 1;");
    let messages = read_until_ready(&mut stream);
    assert!(messages.iter().any(|(kind, _)| *kind == b'D'));
}

#[test]
fn scram_auth_rejects_wrong_password() {
    let options = tritdb_pgwire::ServerOptions {
        auth: tritdb_pgwire::AuthMode::ScramSha256("s3cret".to_string()),
        ..Default::default()
    };
    let (_dir, mut stream) = start_server_with("pgproto-scram-bad", options);
    write_startup_with_user(&mut stream, "tester");

    let (kind, payload) = read_message(&mut stream).expect("SASL");
    assert_eq!(kind, b'R');
    assert_eq!(auth_code(&payload), 10);

    let client_first_bare = "n=,r=clientnonce";
    let client_first = format!("n,,{client_first_bare}");
    let mut initial = Vec::new();
    initial.extend_from_slice(b"SCRAM-SHA-256\0");
    initial.extend_from_slice(&(client_first.len() as i32).to_be_bytes());
    initial.extend_from_slice(client_first.as_bytes());
    write_message(&mut stream, b'p', &initial);

    let (kind, payload) = read_message(&mut stream).expect("SASLContinue");
    assert_eq!(kind, b'R');
    assert_eq!(auth_code(&payload), 11);
    let server_first = String::from_utf8_lossy(&payload[4..]).to_string();
    let mut nonce = String::new();
    let mut salt = Vec::new();
    let mut iterations = 0u32;
    for part in server_first.split(',') {
        if let Some(value) = part.strip_prefix("r=") {
            nonce = value.to_string();
        }
        if let Some(value) = part.strip_prefix("s=") {
            salt = BASE64.decode(value).expect("base64 salt");
        }
        if let Some(value) = part.strip_prefix("i=") {
            iterations = value.parse().expect("iterations");
        }
    }

    // Deliberately derive the proof from the wrong password.
    let salted = pbkdf2_sha256(b"wrong", &salt, iterations);
    let client_key = tritdb_cas::hmac_sha256(&salted, b"Client Key");
    let stored_key = tritdb_cas::sha256(&client_key);
    let client_final_without_proof = format!("c=biws,r={nonce}");
    let auth_message = format!("{client_first_bare},{server_first},{client_final_without_proof}");
    let client_signature = tritdb_cas::hmac_sha256(&stored_key, auth_message.as_bytes());
    let mut proof = [0u8; 32];
    for (index, value) in proof.iter_mut().enumerate() {
        *value = client_key[index] ^ client_signature[index];
    }
    let client_final = format!("{client_final_without_proof},p={}", BASE64.encode(proof));
    write_message(&mut stream, b'p', client_final.as_bytes());

    let messages = read_until_ready(&mut stream);
    assert!(
        has_error(&messages),
        "wrong SCRAM password must be rejected"
    );
}

fn ssl_request_bytes() -> Vec<u8> {
    let mut request = Vec::new();
    request.extend_from_slice(&8i32.to_be_bytes());
    request.extend_from_slice(&80_877_103i32.to_be_bytes());
    request
}

fn startup_bytes() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608i32.to_be_bytes());
    body.push(0);
    let mut message = Vec::new();
    message.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    message.extend_from_slice(&body);
    message
}

fn make_ca() -> (rcgen::Certificate, rcgen::KeyPair) {
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("CA params");
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let key = rcgen::KeyPair::generate().expect("CA key");
    let cert = params.self_signed(&key).expect("CA certificate");
    (cert, key)
}

fn make_client_cert(
    ca_cert: &rcgen::Certificate,
    ca_key: &rcgen::KeyPair,
) -> (rcgen::Certificate, rcgen::KeyPair) {
    let params = rcgen::CertificateParams::new(vec!["client".to_string()]).expect("client params");
    let key = rcgen::KeyPair::generate().expect("client key");
    let cert = params
        .signed_by(&key, ca_cert, ca_key)
        .expect("client certificate");
    (cert, key)
}

fn tls_client_config_with_cert(
    server_cert: &rcgen::Certificate,
    client_cert: &rcgen::Certificate,
    client_key: &rcgen::KeyPair,
) -> std::sync::Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(server_cert.der().clone())
        .expect("trust server certificate");
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let key_der = rustls::pki_types::PrivatePkcs8KeyDer::from(client_key.serialize_der());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("TLS versions")
        .with_root_certificates(roots)
        .with_client_auth_cert(vec![client_cert.der().clone()], key_der.into())
        .expect("client auth");
    std::sync::Arc::new(config)
}

fn tls_client_config_trusting(
    server_cert: &rcgen::Certificate,
) -> std::sync::Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(server_cert.der().clone())
        .expect("trust server certificate");
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    std::sync::Arc::new(config)
}

/// Opens a TLS connection, sends startup + `SELECT 1`, and checks a DataRow.
fn tls_select_one(
    addr: std::net::SocketAddr,
    client_config: std::sync::Arc<rustls::ClientConfig>,
) -> Result<(), String> {
    let mut tcp = TcpStream::connect(addr).map_err(|error| error.to_string())?;
    tcp.set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| error.to_string())?;
    tcp.write_all(&ssl_request_bytes())
        .map_err(|error| error.to_string())?;
    let mut reply = [0u8; 1];
    tcp.read_exact(&mut reply)
        .map_err(|error| error.to_string())?;
    if reply[0] != b'S' {
        return Err(format!("server refused TLS: {}", reply[0] as char));
    }
    let server_name =
        rustls::pki_types::ServerName::try_from("localhost".to_string()).expect("server name");
    let connection = rustls::ClientConnection::new(client_config, server_name)
        .map_err(|error| error.to_string())?;
    let mut tls = rustls::StreamOwned::new(connection, tcp);

    tls.write_all(&startup_bytes())
        .map_err(|error| format!("write startup: {error}"))?;
    let messages = read_until_ready(&mut tls);
    if ready_status(&messages) != Some(b'I') {
        return Err(format!("startup failed: {messages:?}"));
    }
    query(&mut tls, "SELECT 1;");
    let messages = read_until_ready(&mut tls);
    if messages.iter().any(|(kind, _)| *kind == b'D') {
        Ok(())
    } else {
        Err(format!("no DataRow: {messages:?}"))
    }
}

#[test]
fn mtls_accepts_client_certificate_and_rejects_missing_one() {
    let server =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("server cert");
    let (ca_cert, ca_key) = make_ca();
    let (client_cert, client_key) = make_client_cert(&ca_cert, &ca_key);

    let dir = TempDir::new("pgproto-mtls");
    let db = dir.join("mtls.db");
    let cert_path = dir.join("server.pem");
    let key_path = dir.join("server-key.pem");
    let ca_path = dir.join("ca.pem");
    std::fs::write(&cert_path, server.cert.pem()).expect("server cert");
    std::fs::write(&key_path, server.key_pair.serialize_pem()).expect("server key");
    std::fs::write(&ca_path, ca_cert.pem()).expect("CA cert");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let options = std::sync::Arc::new(tritdb_pgwire::ServerOptions {
        tls_cert: Some(cert_path),
        tls_key: Some(key_path),
        tls_client_ca: Some(ca_path),
        require_tls: true,
        ..Default::default()
    });
    thread::spawn(move || {
        let _ = tritdb_pgwire::serve_with_options(db, listener, options);
    });

    // A client certificate issued by the configured CA is accepted.
    let trusted = tls_client_config_with_cert(&server.cert, &client_cert, &client_key);
    tls_select_one(addr, trusted).expect("mTLS handshake with client cert");

    // Without a client certificate the handshake fails.
    let untrusted = tls_client_config_trusting(&server.cert);
    let mut tcp = TcpStream::connect(addr).expect("connect");
    tcp.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    tcp.write_all(&ssl_request_bytes()).expect("ssl request");
    let mut reply = [0u8; 1];
    tcp.read_exact(&mut reply).expect("ssl reply");
    assert_eq!(reply[0], b'S');
    let server_name =
        rustls::pki_types::ServerName::try_from("localhost".to_string()).expect("server name");
    let connection = rustls::ClientConnection::new(untrusted, server_name).expect("client");
    let mut tls = rustls::StreamOwned::new(connection, tcp);
    // TLS 1.3 buffers outgoing data, so the failure surfaces on read.
    let _ = tls.write_all(&startup_bytes());
    let mut buffer = [0u8; 8];
    let outcome = tls.read(&mut buffer);
    assert!(
        matches!(outcome, Err(_) | Ok(0)),
        "a client without a certificate must fail the handshake, got {outcome:?}"
    );
}

#[test]
fn tls_certificate_is_reloaded_when_files_change() {
    let cert1 = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("cert1");
    let cert2 = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("cert2");

    let dir = TempDir::new("pgproto-reload");
    let db = dir.join("reload.db");
    let cert_path = dir.join("server.pem");
    let key_path = dir.join("server-key.pem");
    std::fs::write(&cert_path, cert1.cert.pem()).expect("cert1");
    std::fs::write(&key_path, cert1.key_pair.serialize_pem()).expect("key1");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let options = std::sync::Arc::new(tritdb_pgwire::ServerOptions {
        tls_cert: Some(cert_path.clone()),
        tls_key: Some(key_path.clone()),
        ..Default::default()
    });
    thread::spawn(move || {
        let _ = tritdb_pgwire::serve_with_options(db, listener, options);
    });

    // The initial certificate works.
    tls_select_one(addr, tls_client_config_trusting(&cert1.cert)).expect("first certificate");

    // Rotate the files in place; the next connection must pick them up.
    std::fs::write(&cert_path, cert2.cert.pem()).expect("cert2");
    std::fs::write(&key_path, cert2.key_pair.serialize_pem()).expect("key2");

    tls_select_one(addr, tls_client_config_trusting(&cert2.cert)).expect("reloaded certificate");
    assert!(
        tls_select_one(addr, tls_client_config_trusting(&cert1.cert)).is_err(),
        "the old certificate must no longer be served"
    );
}

#[test]
fn cleartext_password_over_plaintext_requires_opt_in() {
    let dir = TempDir::new("pgproto-plain-guard");
    let db = dir.join("guard.db");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let options = std::sync::Arc::new(tritdb_pgwire::ServerOptions {
        auth: tritdb_pgwire::AuthMode::Password("s3cret".to_string()),
        ..Default::default()
    });
    let error = tritdb_pgwire::serve_with_options(db, listener, options)
        .expect_err("cleartext password without TLS must be refused");
    assert!(
        error.to_string().contains("allow_plaintext_password"),
        "unexpected error: {error}"
    );
}

/// Performs the SCRAM-SHA-256 client exchange. Returns an error when the
/// server rejects the credentials.
fn scram_login<S: Read + Write>(stream: &mut S, password: &str) -> Result<(), String> {
    let (kind, payload) = read_message(stream).ok_or("no AuthenticationSASL")?;
    if kind != b'R' || auth_code(&payload) != 10 {
        return Err(format!("expected AuthenticationSASL, got {kind}"));
    }
    let client_nonce = "clientnonce123456";
    let client_first_bare = format!("n=,r={client_nonce}");
    let client_first = format!("n,,{client_first_bare}");
    let mut initial = Vec::new();
    initial.extend_from_slice(b"SCRAM-SHA-256\0");
    initial.extend_from_slice(&(client_first.len() as i32).to_be_bytes());
    initial.extend_from_slice(client_first.as_bytes());
    write_message(stream, b'p', &initial);

    let (kind, payload) = read_message(stream).ok_or("no SASLContinue")?;
    if kind != b'R' || auth_code(&payload) != 11 {
        return Err(format!("expected SASLContinue, got {kind}"));
    }
    let server_first = String::from_utf8_lossy(&payload[4..]).to_string();
    let mut nonce = String::new();
    let mut salt = Vec::new();
    let mut iterations = 0u32;
    for part in server_first.split(',') {
        if let Some(value) = part.strip_prefix("r=") {
            nonce = value.to_string();
        }
        if let Some(value) = part.strip_prefix("s=") {
            salt = BASE64.decode(value).map_err(|error| error.to_string())?;
        }
        if let Some(value) = part.strip_prefix("i=") {
            iterations = value.parse().map_err(|_| "bad iterations".to_string())?;
        }
    }
    let salted = pbkdf2_sha256(password.as_bytes(), &salt, iterations);
    let client_key = tritdb_cas::hmac_sha256(&salted, b"Client Key");
    let stored_key = tritdb_cas::sha256(&client_key);
    let client_final_without_proof = format!("c=biws,r={nonce}");
    let auth_message = format!("{client_first_bare},{server_first},{client_final_without_proof}");
    let client_signature = tritdb_cas::hmac_sha256(&stored_key, auth_message.as_bytes());
    let mut proof = [0u8; 32];
    for (index, value) in proof.iter_mut().enumerate() {
        *value = client_key[index] ^ client_signature[index];
    }
    let client_final = format!("{client_final_without_proof},p={}", BASE64.encode(proof));
    write_message(stream, b'p', client_final.as_bytes());

    let (kind, payload) = read_message(stream).ok_or("no SASLFinal")?;
    if kind != b'R' || auth_code(&payload) != 12 {
        return Err(format!("expected SASLFinal, got {kind}"));
    }
    Ok(())
}

#[test]
fn scram_verifier_roundtrip_and_login() {
    let verifier =
        tritdb_pgwire::ScramVerifier::from_password("s3cret", 4_096).expect("derive verifier");
    let encoded = verifier.encode();
    let parsed = tritdb_pgwire::ScramVerifier::parse(&encoded).expect("parse verifier");
    assert_eq!(parsed, verifier, "verifier roundtrip");

    let options = tritdb_pgwire::ServerOptions {
        auth: tritdb_pgwire::AuthMode::ScramSha256Verifier(parsed),
        ..Default::default()
    };
    let (_dir, mut stream) = start_server_with("pgproto-scram-verifier", options);
    write_startup_with_user(&mut stream, "tester");
    scram_login(&mut stream, "s3cret").expect("SCRAM login with a persisted verifier");
    let messages = read_until_ready(&mut stream);
    assert!(!has_error(&messages), "login failed: {messages:?}");
    assert_eq!(ready_status(&messages), Some(b'I'));

    query(&mut stream, "SELECT 1;");
    let messages = read_until_ready(&mut stream);
    assert!(messages.iter().any(|(kind, _)| *kind == b'D'));
}

#[test]
fn scram_verifier_rejects_wrong_password() {
    let verifier =
        tritdb_pgwire::ScramVerifier::from_password("s3cret", 4_096).expect("derive verifier");
    let options = tritdb_pgwire::ServerOptions {
        auth: tritdb_pgwire::AuthMode::ScramSha256Verifier(verifier),
        ..Default::default()
    };
    let (_dir, mut stream) = start_server_with("pgproto-scram-verifier-bad", options);
    write_startup_with_user(&mut stream, "tester");
    assert!(
        scram_login(&mut stream, "wrong").is_err(),
        "wrong password must be rejected"
    );
    let messages = read_until_ready(&mut stream);
    assert!(has_error(&messages) || messages.is_empty());
}

#[test]
fn revoked_client_certificate_is_rejected() {
    let server =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("server cert");
    let (ca_cert, ca_key) = make_ca();
    let (client_cert, client_key) = make_client_cert(&ca_cert, &ca_key);

    let dir = TempDir::new("pgproto-revoked");
    let db = dir.join("revoked.db");
    let cert_path = dir.join("server.pem");
    let key_path = dir.join("server-key.pem");
    let ca_path = dir.join("ca.pem");
    let revoked_path = dir.join("revoked.txt");
    std::fs::write(&cert_path, server.cert.pem()).expect("server cert");
    std::fs::write(&key_path, server.key_pair.serialize_pem()).expect("server key");
    std::fs::write(&ca_path, ca_cert.pem()).expect("CA cert");

    let fingerprint = tritdb_cas::sha256(client_cert.der().as_ref());
    let mut revoked = String::from("# revoked client certificates\n");
    for byte in fingerprint {
        revoked.push_str(&format!("{byte:02x}"));
    }
    revoked.push('\n');
    std::fs::write(&revoked_path, revoked).expect("revocation list");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let options = std::sync::Arc::new(tritdb_pgwire::ServerOptions {
        tls_cert: Some(cert_path),
        tls_key: Some(key_path),
        tls_client_ca: Some(ca_path),
        tls_client_revoked: Some(revoked_path),
        require_tls: true,
        ..Default::default()
    });
    thread::spawn(move || {
        let _ = tritdb_pgwire::serve_with_options(db, listener, options);
    });

    let client = tls_client_config_with_cert(&server.cert, &client_cert, &client_key);
    assert!(
        tls_select_one(addr, client).is_err(),
        "a revoked client certificate must be rejected"
    );
}

fn parse_format_codes(payload: &[u8]) -> Vec<i16> {
    let count = i16::from_be_bytes([payload[0], payload[1]]) as usize;
    let mut cursor = 2;
    let mut codes = Vec::new();
    for _ in 0..count {
        let end = payload[cursor..]
            .iter()
            .position(|byte| *byte == 0)
            .expect("field name terminator")
            + cursor;
        // name, table oid (4), attribute number (2), type oid (4), size (2),
        // type modifier (4), format code (2).
        let at = end + 1 + 4 + 2 + 4 + 2 + 4;
        codes.push(i16::from_be_bytes([payload[at], payload[at + 1]]));
        cursor = at + 2;
    }
    codes
}

fn parse_binary_row(payload: &[u8]) -> Vec<Option<Vec<u8>>> {
    let count = i16::from_be_bytes([payload[0], payload[1]]) as usize;
    let mut cursor = 2;
    let mut values = Vec::new();
    for _ in 0..count {
        let length = i32::from_be_bytes([
            payload[cursor],
            payload[cursor + 1],
            payload[cursor + 2],
            payload[cursor + 3],
        ]);
        cursor += 4;
        if length < 0 {
            values.push(None);
        } else {
            let length = length as usize;
            values.push(Some(payload[cursor..cursor + length].to_vec()));
            cursor += length;
        }
    }
    values
}

#[test]
fn copy_from_stdin_inserts_rows() {
    let (_dir, mut stream) = start_server("pgproto-copy-in");
    write_startup(&mut stream);
    let _ = read_until_ready(&mut stream);

    query(
        &mut stream,
        "CREATE TABLE t (id INT NOT NULL, name TEXT, flag BOOL);",
    );
    let _ = read_until_ready(&mut stream);

    query(&mut stream, "COPY t FROM STDIN;");
    let (kind, payload) = read_message(&mut stream).expect("CopyInResponse");
    assert_eq!(kind, b'G');
    assert_eq!(payload[0], 0, "COPY text format");
    assert_eq!(i16::from_be_bytes([payload[1], payload[2]]), 3);

    write_message(&mut stream, b'd', b"1\talpha\tt\n");
    write_message(&mut stream, b'd', b"2\t\\N\tf\n");
    write_message(&mut stream, b'c', b"");

    let messages = read_until_ready(&mut stream);
    assert!(
        messages
            .iter()
            .any(|(kind, payload)| *kind == b'C' && payload.starts_with(b"COPY 2")),
        "missing COPY 2 tag: {messages:?}"
    );
    assert_eq!(ready_status(&messages), Some(b'I'));

    query(&mut stream, "SELECT id, name, flag FROM t ORDER BY id;");
    let messages = read_until_ready(&mut stream);
    let rows: Vec<Vec<Option<String>>> = messages
        .iter()
        .filter(|(kind, _)| *kind == b'D')
        .map(|(_, payload)| parse_row(payload))
        .collect();
    assert_eq!(
        rows,
        vec![
            vec![Some("1".into()), Some("alpha".into()), Some("t".into())],
            vec![Some("2".into()), None, Some("f".into())],
        ]
    );
}

#[test]
fn copy_to_stdout_emits_text_rows() {
    let (_dir, mut stream) = start_server("pgproto-copy-out");
    write_startup(&mut stream);
    let _ = read_until_ready(&mut stream);

    query(
        &mut stream,
        "CREATE TABLE t (id INT NOT NULL, name TEXT); \
         INSERT INTO t VALUES (1, 'a'), (2, NULL);",
    );
    let _ = read_until_ready(&mut stream);

    query(&mut stream, "COPY t TO STDOUT;");
    let (kind, payload) = read_message(&mut stream).expect("CopyOutResponse");
    assert_eq!(kind, b'H');
    assert_eq!(i16::from_be_bytes([payload[1], payload[2]]), 2);

    let mut lines = Vec::new();
    loop {
        let (kind, payload) = read_message(&mut stream).expect("copy message");
        match kind {
            b'd' => lines.push(String::from_utf8_lossy(&payload).to_string()),
            b'c' => break,
            other => panic!("unexpected message {} during COPY OUT", other as char),
        }
    }
    assert_eq!(lines, vec!["1\ta\n".to_string(), "2\t\\N\n".to_string()]);

    let messages = read_until_ready(&mut stream);
    assert!(messages
        .iter()
        .any(|(kind, payload)| *kind == b'C' && payload.starts_with(b"COPY 2")));
    assert_eq!(ready_status(&messages), Some(b'I'));
}

#[test]
fn extended_query_returns_binary_columns() {
    let (_dir, mut stream) = start_server("pgproto-binary");
    write_startup(&mut stream);
    let _ = read_until_ready(&mut stream);

    query(
        &mut stream,
        "CREATE TABLE t (id INT NOT NULL, flag BOOL, name TEXT); \
         INSERT INTO t VALUES (7, TRUE, 'x');",
    );
    let _ = read_until_ready(&mut stream);

    // Parse: SELECT id, flag, name FROM t
    let mut payload = Vec::new();
    payload.extend_from_slice(b"stmt\0");
    payload.extend_from_slice(b"SELECT id, flag, name FROM t;\0");
    payload.extend_from_slice(&0i16.to_be_bytes());
    write_message(&mut stream, b'P', &payload);

    // Bind with one result format code: 1 = binary.
    let mut payload = Vec::new();
    payload.extend_from_slice(b"portal\0stmt\0");
    payload.extend_from_slice(&0i16.to_be_bytes()); // parameter format count
    payload.extend_from_slice(&0i16.to_be_bytes()); // parameter count
    payload.extend_from_slice(&1i16.to_be_bytes()); // result format count
    payload.extend_from_slice(&1i16.to_be_bytes()); // binary
    write_message(&mut stream, b'B', &payload);
    write_message(&mut stream, b'S', b"");
    let _ = read_until_ready(&mut stream);

    // Describe portal: the RowDescription must advertise binary columns.
    let mut payload = b"P".to_vec();
    payload.extend_from_slice(b"portal\0");
    write_message(&mut stream, b'D', &payload);
    write_message(&mut stream, b'S', b"");
    let messages = read_until_ready(&mut stream);
    let description = messages
        .iter()
        .find(|(kind, _)| *kind == b'T')
        .map(|(_, payload)| payload.clone())
        .expect("RowDescription");
    assert_eq!(parse_format_codes(&description), vec![1, 1, 1]);

    // Execute: int8 is 8 big-endian bytes, bool is one byte, text is UTF-8.
    let mut payload = b"portal\0".to_vec();
    payload.extend_from_slice(&0i32.to_be_bytes());
    write_message(&mut stream, b'E', &payload);
    write_message(&mut stream, b'S', b"");
    let messages = read_until_ready(&mut stream);
    let data = messages
        .iter()
        .find(|(kind, _)| *kind == b'D')
        .map(|(_, payload)| payload.clone())
        .expect("DataRow");
    assert_eq!(
        parse_binary_row(&data),
        vec![
            Some(7i64.to_be_bytes().to_vec()),
            Some(vec![1u8]),
            Some(b"x".to_vec()),
        ]
    );
}

#[test]
fn simple_query_stays_text_format() {
    let (_dir, mut stream) = start_server("pgproto-text");
    write_startup(&mut stream);
    let _ = read_until_ready(&mut stream);

    query(&mut stream, "SELECT 1, TRUE, 'x';");
    let messages = read_until_ready(&mut stream);
    let description = messages
        .iter()
        .find(|(kind, _)| *kind == b'T')
        .map(|(_, payload)| payload.clone())
        .expect("RowDescription");
    assert_eq!(parse_format_codes(&description), vec![0, 0, 0]);
    let row = messages
        .iter()
        .find(|(kind, _)| *kind == b'D')
        .map(|(_, payload)| parse_row(payload))
        .expect("DataRow");
    assert_eq!(
        row,
        vec![Some("1".into()), Some("t".into()), Some("x".into())]
    );
}

#[test]
fn extended_query_accepts_binary_parameters() {
    let (_dir, mut stream) = start_server("pgproto-binary-params");
    write_startup(&mut stream);
    let _ = read_until_ready(&mut stream);

    query(
        &mut stream,
        "CREATE TABLE t (id INT NOT NULL, flag BOOL, name TEXT);",
    );
    let _ = read_until_ready(&mut stream);

    // Parse with explicit parameter OIDs: int8, bool, text.
    let mut payload = Vec::new();
    payload.extend_from_slice(b"stmt\0");
    payload.extend_from_slice(b"INSERT INTO t VALUES ($1, $2, $3);\0");
    payload.extend_from_slice(&3i16.to_be_bytes());
    for oid in [20i32, 16, 25] {
        payload.extend_from_slice(&oid.to_be_bytes());
    }
    write_message(&mut stream, b'P', &payload);

    // Bind with one parameter format code (1 = binary).
    let mut payload = Vec::new();
    payload.extend_from_slice(b"portal\0stmt\0");
    payload.extend_from_slice(&1i16.to_be_bytes()); // parameter format count
    payload.extend_from_slice(&1i16.to_be_bytes()); // binary
    payload.extend_from_slice(&3i16.to_be_bytes()); // parameter count
    payload.extend_from_slice(&8i32.to_be_bytes());
    payload.extend_from_slice(&42i64.to_be_bytes());
    payload.extend_from_slice(&1i32.to_be_bytes());
    payload.push(1);
    payload.extend_from_slice(&3i32.to_be_bytes());
    payload.extend_from_slice(b"bin");
    payload.extend_from_slice(&0i16.to_be_bytes()); // result format count
    write_message(&mut stream, b'B', &payload);

    let mut execute = b"portal\0".to_vec();
    execute.extend_from_slice(&0i32.to_be_bytes());
    write_message(&mut stream, b'E', &execute);
    write_message(&mut stream, b'S', b"");
    let messages = read_until_ready(&mut stream);
    assert!(
        !has_error(&messages),
        "binary parameter bind failed: {messages:?}"
    );

    query(&mut stream, "SELECT id, flag, name FROM t;");
    let messages = read_until_ready(&mut stream);
    let row = messages
        .iter()
        .find(|(kind, _)| *kind == b'D')
        .map(|(_, payload)| parse_row(payload))
        .expect("DataRow");
    assert_eq!(
        row,
        vec![Some("42".into()), Some("t".into()), Some("bin".into())]
    );
}

#[test]
fn copy_csv_roundtrip() {
    let (_dir, mut stream) = start_server("pgproto-copy-csv");
    write_startup(&mut stream);
    let _ = read_until_ready(&mut stream);

    query(
        &mut stream,
        "CREATE TABLE t (id INT NOT NULL, name TEXT, flag BOOL);",
    );
    let _ = read_until_ready(&mut stream);

    query(&mut stream, "COPY t FROM STDIN (FORMAT csv);");
    let (kind, payload) = read_message(&mut stream).expect("CopyInResponse");
    assert_eq!(kind, b'G');
    assert_eq!(payload[0], 0, "CSV uses the text wire format");

    write_message(&mut stream, b'd', b"1,alpha,t\n");
    write_message(&mut stream, b'd', b"2,,f\n");
    write_message(&mut stream, b'c', b"");
    let messages = read_until_ready(&mut stream);
    assert!(
        messages
            .iter()
            .any(|(kind, payload)| *kind == b'C' && payload.starts_with(b"COPY 2")),
        "missing COPY 2: {messages:?}"
    );

    query(&mut stream, "SELECT id, name, flag FROM t ORDER BY id;");
    let messages = read_until_ready(&mut stream);
    let rows: Vec<Vec<Option<String>>> = messages
        .iter()
        .filter(|(kind, _)| *kind == b'D')
        .map(|(_, payload)| parse_row(payload))
        .collect();
    assert_eq!(
        rows,
        vec![
            vec![Some("1".into()), Some("alpha".into()), Some("t".into())],
            vec![Some("2".into()), None, Some("f".into())],
        ]
    );

    query(&mut stream, "COPY t TO STDOUT (FORMAT csv);");
    let (kind, _) = read_message(&mut stream).expect("CopyOutResponse");
    assert_eq!(kind, b'H');
    let mut lines = Vec::new();
    loop {
        let (kind, payload) = read_message(&mut stream).expect("copy message");
        match kind {
            b'd' => lines.push(String::from_utf8_lossy(&payload).to_string()),
            b'c' => break,
            other => panic!("unexpected message {} during COPY OUT", other as char),
        }
    }
    assert_eq!(lines, vec!["1,alpha,t\n".to_string(), "2,,f\n".to_string()]);
    let _ = read_until_ready(&mut stream);
}

#[test]
fn copy_binary_roundtrip() {
    let (_dir, mut stream) = start_server("pgproto-copy-binary");
    write_startup(&mut stream);
    let _ = read_until_ready(&mut stream);

    query(
        &mut stream,
        "CREATE TABLE t (id INT NOT NULL, flag BOOL, name TEXT);",
    );
    let _ = read_until_ready(&mut stream);

    query(&mut stream, "COPY t FROM STDIN (FORMAT binary);");
    let (kind, payload) = read_message(&mut stream).expect("CopyInResponse");
    assert_eq!(kind, b'G');
    assert_eq!(payload[0], 1, "binary wire format");

    let mut data = Vec::new();
    data.extend_from_slice(b"PGCOPY\n\xff\r\n\0");
    data.extend_from_slice(&0i32.to_be_bytes());
    data.extend_from_slice(&0i32.to_be_bytes());
    data.extend_from_slice(&3i16.to_be_bytes());
    data.extend_from_slice(&8i32.to_be_bytes());
    data.extend_from_slice(&42i64.to_be_bytes());
    data.extend_from_slice(&1i32.to_be_bytes());
    data.push(1);
    data.extend_from_slice(&3i32.to_be_bytes());
    data.extend_from_slice(b"bin");
    data.extend_from_slice(&(-1i16).to_be_bytes());
    write_message(&mut stream, b'd', &data);
    write_message(&mut stream, b'c', b"");
    let messages = read_until_ready(&mut stream);
    assert!(
        messages
            .iter()
            .any(|(kind, payload)| *kind == b'C' && payload.starts_with(b"COPY 1")),
        "missing COPY 1: {messages:?}"
    );

    query(&mut stream, "SELECT id, flag, name FROM t;");
    let messages = read_until_ready(&mut stream);
    let rows: Vec<Vec<Option<String>>> = messages
        .iter()
        .filter(|(kind, _)| *kind == b'D')
        .map(|(_, payload)| parse_row(payload))
        .collect();
    assert_eq!(
        rows,
        vec![vec![
            Some("42".into()),
            Some("t".into()),
            Some("bin".into())
        ]]
    );

    query(&mut stream, "COPY t TO STDOUT (FORMAT binary);");
    let (kind, payload) = read_message(&mut stream).expect("CopyOutResponse");
    assert_eq!(kind, b'H');
    assert_eq!(payload[0], 1);
    let mut data = Vec::new();
    loop {
        let (kind, payload) = read_message(&mut stream).expect("copy message");
        match kind {
            b'd' => data.extend_from_slice(&payload),
            b'c' => break,
            other => panic!("unexpected message {} during COPY OUT", other as char),
        }
    }
    assert!(data.starts_with(b"PGCOPY\n\xff\r\n\0"));
    assert_eq!(&data[19..21], &3i16.to_be_bytes());
    assert_eq!(&data[21..25], &8i32.to_be_bytes());
    assert_eq!(&data[25..33], &42i64.to_be_bytes());
    assert_eq!(&data[33..37], &1i32.to_be_bytes());
    assert_eq!(data[37], 1);
    assert_eq!(&data[38..42], &3i32.to_be_bytes());
    assert_eq!(&data[42..45], b"bin");
    assert_eq!(&data[45..47], &(-1i16).to_be_bytes());
    let _ = read_until_ready(&mut stream);
}

fn make_client_cert_with_serial(
    ca_cert: &rcgen::Certificate,
    ca_key: &rcgen::KeyPair,
    serial: u64,
) -> (rcgen::Certificate, rcgen::KeyPair) {
    let mut params =
        rcgen::CertificateParams::new(vec!["client".to_string()]).expect("client params");
    params.serial_number = Some(rcgen::SerialNumber::from(serial));
    let key = rcgen::KeyPair::generate().expect("client key");
    let cert = params
        .signed_by(&key, ca_cert, ca_key)
        .expect("client certificate");
    (cert, key)
}

#[test]
fn crl_signed_by_ca_revokes_client_certificate() {
    let server =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("server cert");
    let (ca_cert, ca_key) = make_ca();
    let (client_cert, client_key) = make_client_cert_with_serial(&ca_cert, &ca_key, 42);

    let dir = TempDir::new("pgproto-crl");
    let db = dir.join("crl.db");
    let cert_path = dir.join("server.pem");
    let key_path = dir.join("server-key.pem");
    let ca_path = dir.join("ca.pem");
    let crl_path = dir.join("crl.pem");
    std::fs::write(&cert_path, server.cert.pem()).expect("server cert");
    std::fs::write(&key_path, server.key_pair.serialize_pem()).expect("server key");
    std::fs::write(&ca_path, ca_cert.pem()).expect("CA cert");

    let crl = rcgen::CertificateRevocationListParams {
        this_update: rcgen::date_time_ymd(2024, 1, 1),
        next_update: rcgen::date_time_ymd(2030, 1, 1),
        crl_number: rcgen::SerialNumber::from(1u64),
        issuing_distribution_point: None,
        revoked_certs: vec![rcgen::RevokedCertParams {
            serial_number: rcgen::SerialNumber::from(42u64),
            revocation_time: rcgen::date_time_ymd(2024, 1, 1),
            reason_code: Some(rcgen::RevocationReason::KeyCompromise),
            invalidity_date: None,
        }],
        key_identifier_method: rcgen::KeyIdMethod::Sha256,
    }
    .signed_by(&ca_cert, &ca_key)
    .expect("CRL");
    std::fs::write(&crl_path, crl.pem().expect("CRL PEM")).expect("CRL file");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let options = std::sync::Arc::new(tritdb_pgwire::ServerOptions {
        tls_cert: Some(cert_path),
        tls_key: Some(key_path),
        tls_client_ca: Some(ca_path),
        tls_client_revoked: Some(crl_path),
        require_tls: true,
        ..Default::default()
    });
    thread::spawn(move || {
        let _ = tritdb_pgwire::serve_with_options(db, listener, options);
    });

    let client = tls_client_config_with_cert(&server.cert, &client_cert, &client_key);
    assert!(
        tls_select_one(addr, client).is_err(),
        "a CRL-revoked client certificate must be rejected"
    );
}

#[test]
fn crl_without_revoked_serial_still_accepts_client() {
    let server =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("server cert");
    let (ca_cert, ca_key) = make_ca();
    let (client_cert, client_key) = make_client_cert_with_serial(&ca_cert, &ca_key, 7);

    let dir = TempDir::new("pgproto-crl-ok");
    let db = dir.join("crl-ok.db");
    let cert_path = dir.join("server.pem");
    let key_path = dir.join("server-key.pem");
    let ca_path = dir.join("ca.pem");
    let crl_path = dir.join("crl.pem");
    std::fs::write(&cert_path, server.cert.pem()).expect("server cert");
    std::fs::write(&key_path, server.key_pair.serialize_pem()).expect("server key");
    std::fs::write(&ca_path, ca_cert.pem()).expect("CA cert");

    // Revokes a different serial number.
    let crl = rcgen::CertificateRevocationListParams {
        this_update: rcgen::date_time_ymd(2024, 1, 1),
        next_update: rcgen::date_time_ymd(2030, 1, 1),
        crl_number: rcgen::SerialNumber::from(2u64),
        issuing_distribution_point: None,
        revoked_certs: vec![rcgen::RevokedCertParams {
            serial_number: rcgen::SerialNumber::from(99u64),
            revocation_time: rcgen::date_time_ymd(2024, 1, 1),
            reason_code: Some(rcgen::RevocationReason::Superseded),
            invalidity_date: None,
        }],
        key_identifier_method: rcgen::KeyIdMethod::Sha256,
    }
    .signed_by(&ca_cert, &ca_key)
    .expect("CRL");
    std::fs::write(&crl_path, crl.pem().expect("CRL PEM")).expect("CRL file");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let options = std::sync::Arc::new(tritdb_pgwire::ServerOptions {
        tls_cert: Some(cert_path),
        tls_key: Some(key_path),
        tls_client_ca: Some(ca_path),
        tls_client_revoked: Some(crl_path),
        require_tls: true,
        ..Default::default()
    });
    thread::spawn(move || {
        let _ = tritdb_pgwire::serve_with_options(db, listener, options);
    });

    let client = tls_client_config_with_cert(&server.cert, &client_cert, &client_key);
    tls_select_one(addr, client).expect("a client with an unrevoked serial must be accepted");
}

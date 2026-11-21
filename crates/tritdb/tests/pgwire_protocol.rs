//! Protocol-level robustness tests for the minimal PostgreSQL wire server:
//! startup edge cases, unknown message types, malformed lengths, Close/Bind on
//! unknown names, repeated Sync and transaction-state transitions.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

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

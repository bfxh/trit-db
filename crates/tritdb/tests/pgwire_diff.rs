//! Differential probes against a real PostgreSQL server.
//!
//! Run with a live server:
//!
//! ```text
//! TRITDB_PG_HOST=127.0.0.1 TRITDB_PG_PORT=5432 \
//!   cargo test -p tritdb --test pgwire_diff -- --ignored --nocapture
//! ```
//!
//! The CI `diff` job provides a `postgres:16` service for that command. Without
//! `TRITDB_PG_HOST` the test returns immediately, so plain `cargo test` stays
//! green on machines without PostgreSQL.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

use tritdb_testkit::TempDir;

fn write_startup<S: Write>(stream: &mut S, user: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608i32.to_be_bytes());
    body.extend_from_slice(b"user\0");
    body.extend_from_slice(user.as_bytes());
    body.push(0);
    body.extend_from_slice(b"database\0");
    body.extend_from_slice(user.as_bytes());
    body.push(0);
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
    while let Some(message) = read_message(stream) {
        let ready = message.0 == b'Z';
        messages.push(message);
        if ready {
            break;
        }
    }
    messages
}

fn query<S: Write>(stream: &mut S, sql: &str) {
    let mut payload = sql.as_bytes().to_vec();
    payload.push(0);
    write_message(stream, b'Q', &payload);
}

/// SQLSTATE from the first ErrorResponse (`C` field).
fn error_code(messages: &[(u8, Vec<u8>)]) -> Option<String> {
    let payload = messages
        .iter()
        .find(|(kind, _)| *kind == b'E')
        .map(|(_, payload)| payload)?;
    let mut offset = 0usize;
    while offset < payload.len() {
        let field = payload[offset];
        offset += 1;
        if field == 0 {
            break;
        }
        let end = payload[offset..].iter().position(|byte| *byte == 0)? + offset;
        let value = String::from_utf8_lossy(&payload[offset..end]).to_string();
        if field == b'C' {
            return Some(value);
        }
        offset = end + 1;
    }
    None
}

/// CopyData payloads concatenated (the COPY text-format body).
fn copy_text(messages: &[(u8, Vec<u8>)]) -> String {
    let mut out = String::new();
    for (kind, payload) in messages {
        if *kind == b'd' {
            out.push_str(&String::from_utf8_lossy(payload));
        }
    }
    out
}

fn connect_pg(host: &str, port: u16) -> TcpStream {
    let mut stream = TcpStream::connect((host, port)).expect("connect postgres");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .expect("read timeout");
    write_startup(&mut stream, "postgres");
    let messages = read_until_ready(&mut stream);
    assert!(
        !messages.iter().any(|(kind, _)| *kind == b'E'),
        "postgres rejected startup: {messages:?}"
    );
    stream
}

fn start_local() -> (TempDir, TcpStream) {
    let dir = TempDir::new("pgwire-diff");
    let db = dir.join("diff.db");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let db_path = db.clone();
    thread::spawn(move || {
        let _ = tritdb_pgwire::serve(db_path, listener);
    });
    let mut stream = TcpStream::connect(addr).expect("connect local");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .expect("read timeout");
    write_startup(&mut stream, "postgres");
    let messages = read_until_ready(&mut stream);
    assert!(
        !messages.iter().any(|(kind, _)| *kind == b'E'),
        "local server rejected startup: {messages:?}"
    );
    (dir, stream)
}

#[test]
#[ignore = "needs a real PostgreSQL at TRITDB_PG_HOST/TRITDB_PG_PORT"]
fn pgwire_matches_postgres_on_error_code_and_copy() {
    let Ok(host) = std::env::var("TRITDB_PG_HOST") else {
        return;
    };
    let port: u16 = std::env::var("TRITDB_PG_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(5432);
    let mut pg = connect_pg(&host, port);
    let (_dir, mut local) = start_local();

    // Division by zero must map to the same SQLSTATE (22012).
    query(&mut pg, "SELECT 1/0;");
    let pg_error = error_code(&read_until_ready(&mut pg)).expect("postgres error");
    query(&mut local, "SELECT 1/0;");
    let local_error = error_code(&read_until_ready(&mut local)).expect("local error");
    assert_eq!(pg_error, local_error, "SQLSTATE mismatch for 1/0");

    // COPY ... TO STDOUT text format must match byte for byte.
    let setup = "CREATE TABLE diff_probe (x INT NOT NULL); \
                 INSERT INTO diff_probe VALUES (1), (2);";
    query(&mut pg, setup);
    let _ = read_until_ready(&mut pg);
    query(&mut pg, "COPY diff_probe TO STDOUT;");
    let pg_copy = copy_text(&read_until_ready(&mut pg));

    query(&mut local, setup);
    let _ = read_until_ready(&mut local);
    query(&mut local, "COPY diff_probe TO STDOUT;");
    let local_copy = copy_text(&read_until_ready(&mut local));

    assert_eq!(pg_copy, local_copy, "COPY TO STDOUT text mismatch");
}

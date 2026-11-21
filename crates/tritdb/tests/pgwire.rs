//! End-to-end tests for the minimal PostgreSQL wire protocol server.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

use tritdb_testkit::TempDir;

fn startup(stream: &mut TcpStream) {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608i32.to_be_bytes());
    body.push(0);
    let mut message = Vec::new();
    message.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    message.extend_from_slice(&body);
    stream.write_all(&message).expect("startup");
}

fn query(stream: &mut TcpStream, sql: &str) {
    let mut payload = sql.as_bytes().to_vec();
    payload.push(0);
    let mut message = vec![b'Q'];
    message.extend_from_slice(&((payload.len() + 4) as i32).to_be_bytes());
    message.extend_from_slice(&payload);
    stream.write_all(&message).expect("query");
}

fn read_message(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut kind = [0u8; 1];
    stream.read_exact(&mut kind).expect("message type");
    let mut length = [0u8; 4];
    stream.read_exact(&mut length).expect("message length");
    let length = i32::from_be_bytes(length) as usize - 4;
    let mut payload = vec![0u8; length];
    stream.read_exact(&mut payload).expect("message payload");
    (kind[0], payload)
}

fn read_until_ready(stream: &mut TcpStream) -> Vec<(u8, Vec<u8>)> {
    let mut messages = Vec::new();
    loop {
        let (kind, payload) = read_message(stream);
        messages.push((kind, payload));
        if kind == b'Z' {
            return messages;
        }
    }
}

fn parse_fields(payload: &[u8]) -> Vec<String> {
    let count = i16::from_be_bytes([payload[0], payload[1]]) as usize;
    let mut cursor = 2;
    let mut names = Vec::new();
    for _ in 0..count {
        let end = payload[cursor..]
            .iter()
            .position(|byte| *byte == 0)
            .expect("field terminator")
            + cursor;
        names.push(String::from_utf8_lossy(&payload[cursor..end]).to_string());
        cursor = end + 1 + 18;
    }
    names
}

/// Parses `(type oid, type size)` pairs from a `RowDescription` payload.
fn parse_type_oids(payload: &[u8]) -> Vec<(i32, i16)> {
    let count = i16::from_be_bytes([payload[0], payload[1]]) as usize;
    let mut cursor = 2;
    let mut types = Vec::new();
    for _ in 0..count {
        let end = payload[cursor..]
            .iter()
            .position(|byte| *byte == 0)
            .expect("field terminator")
            + cursor;
        let mut at = end + 1;
        at += 4; // table oid
        at += 2; // attribute number
        let oid = i32::from_be_bytes([
            payload[at],
            payload[at + 1],
            payload[at + 2],
            payload[at + 3],
        ]);
        let size = i16::from_be_bytes([payload[at + 4], payload[at + 5]]);
        types.push((oid, size));
        cursor = end + 1 + 18;
    }
    types
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

#[test]
fn simple_query_roundtrip() {
    let dir = TempDir::new("pgwire");
    let db = dir.join("pg.db");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let db_path = db.clone();
    thread::spawn(move || {
        let _ = tritdb_pgwire::serve(db_path, listener);
    });

    let mut stream = TcpStream::connect(addr).expect("connect");
    startup(&mut stream);
    let messages = read_until_ready(&mut stream);
    assert!(
        messages.iter().any(|(kind, _)| *kind == b'R'),
        "missing AuthenticationOk"
    );
    assert!(
        messages.iter().any(|(kind, _)| *kind == b'Z'),
        "missing ReadyForQuery"
    );

    query(
        &mut stream,
        "CREATE TABLE t (id INT NOT NULL, name TEXT); INSERT INTO t VALUES (1,'a'),(2,'b');",
    );
    let messages = read_until_ready(&mut stream);
    let tags: Vec<String> = messages
        .iter()
        .filter(|(kind, _)| *kind == b'C')
        .map(|(_, payload)| String::from_utf8_lossy(&payload[..payload.len() - 1]).to_string())
        .collect();
    assert_eq!(tags, vec!["CREATE TABLE", "INSERT 0 2"], "tags: {tags:?}");

    query(&mut stream, "SELECT id, name FROM t ORDER BY id;");
    let messages = read_until_ready(&mut stream);
    let description = messages
        .iter()
        .find(|(kind, _)| *kind == b'T')
        .expect("row description");
    assert_eq!(parse_fields(&description.1), vec!["id", "name"]);

    let rows: Vec<Vec<Option<String>>> = messages
        .iter()
        .filter(|(kind, _)| *kind == b'D')
        .map(|(_, payload)| parse_row(payload))
        .collect();
    assert_eq!(
        rows,
        vec![
            vec![Some("1".to_string()), Some("a".to_string())],
            vec![Some("2".to_string()), Some("b".to_string())],
        ]
    );

    // SQL errors become ErrorResponse + ReadyForQuery.
    query(&mut stream, "SELECT * FROM missing;");
    let messages = read_until_ready(&mut stream);
    assert!(
        messages.iter().any(|(kind, _)| *kind == b'E'),
        "missing ErrorResponse"
    );

    stream.write_all(b"X\x00\x00\x00\x04").expect("terminate");
}

fn write_framed(stream: &mut TcpStream, kind: u8, payload: &[u8]) {
    let mut message = vec![kind];
    message.extend_from_slice(&((payload.len() + 4) as i32).to_be_bytes());
    message.extend_from_slice(payload);
    stream.write_all(&message).expect("write message");
}

fn write_parse(stream: &mut TcpStream, statement: &str, sql: &str, param_oids: &[i32]) {
    let mut payload = Vec::new();
    payload.extend_from_slice(statement.as_bytes());
    payload.push(0);
    payload.extend_from_slice(sql.as_bytes());
    payload.push(0);
    payload.extend_from_slice(&(param_oids.len() as i16).to_be_bytes());
    for oid in param_oids {
        payload.extend_from_slice(&oid.to_be_bytes());
    }
    write_framed(stream, b'P', &payload);
}

fn write_bind(stream: &mut TcpStream, portal: &str, statement: &str, params: &[Option<&[u8]>]) {
    let mut payload = Vec::new();
    payload.extend_from_slice(portal.as_bytes());
    payload.push(0);
    payload.extend_from_slice(statement.as_bytes());
    payload.push(0);
    payload.extend_from_slice(&0i16.to_be_bytes()); // no parameter format codes
    payload.extend_from_slice(&(params.len() as i16).to_be_bytes());
    for param in params {
        match param {
            None => payload.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(value) => {
                payload.extend_from_slice(&(value.len() as i32).to_be_bytes());
                payload.extend_from_slice(value);
            }
        }
    }
    payload.extend_from_slice(&0i16.to_be_bytes()); // no result format codes
    write_framed(stream, b'B', &payload);
}

fn write_describe_statement(stream: &mut TcpStream, statement: &str) {
    let mut payload = vec![b'S'];
    payload.extend_from_slice(statement.as_bytes());
    payload.push(0);
    write_framed(stream, b'D', &payload);
}

fn write_describe_portal(stream: &mut TcpStream, portal: &str) {
    let mut payload = vec![b'P'];
    payload.extend_from_slice(portal.as_bytes());
    payload.push(0);
    write_framed(stream, b'D', &payload);
}

fn write_execute(stream: &mut TcpStream, portal: &str) {
    let mut payload = Vec::new();
    payload.extend_from_slice(portal.as_bytes());
    payload.push(0);
    payload.extend_from_slice(&0i32.to_be_bytes()); // no row limit
    write_framed(stream, b'E', &payload);
}

fn write_sync(stream: &mut TcpStream) {
    write_framed(stream, b'S', &[]);
}

#[test]
fn extended_query_roundtrip() {
    let dir = TempDir::new("pgwire-extended");
    let db = dir.join("pg.db");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let db_path = db.clone();
    thread::spawn(move || {
        let _ = tritdb_pgwire::serve(db_path, listener);
    });

    let mut stream = TcpStream::connect(addr).expect("connect");
    startup(&mut stream);
    let _ = read_until_ready(&mut stream);

    query(
        &mut stream,
        "CREATE TABLE t (id INT NOT NULL, name TEXT); INSERT INTO t VALUES (1,'a'),(2,'b'),(3,'c');",
    );
    let _ = read_until_ready(&mut stream);

    write_parse(
        &mut stream,
        "",
        "SELECT id, name FROM t WHERE id > $1 ORDER BY id;",
        &[20],
    );
    write_bind(&mut stream, "", "", &[Some(b"1")]);
    write_describe_statement(&mut stream, "");
    write_describe_portal(&mut stream, "");
    write_execute(&mut stream, "");
    write_sync(&mut stream);

    let messages = read_until_ready(&mut stream);
    let kinds: Vec<u8> = messages.iter().map(|(kind, _)| *kind).collect();
    assert!(kinds.contains(&b'1'), "missing ParseComplete: {kinds:?}");
    assert!(kinds.contains(&b'2'), "missing BindComplete: {kinds:?}");
    assert!(
        kinds.contains(&b't'),
        "missing ParameterDescription: {kinds:?}"
    );
    let descriptions: Vec<Vec<String>> = messages
        .iter()
        .filter(|(kind, _)| *kind == b'T')
        .map(|(_, payload)| parse_fields(payload))
        .collect();
    assert!(
        descriptions.iter().any(|fields| fields == &["id", "name"]),
        "missing RowDescription for id/name: {descriptions:?}"
    );

    let rows: Vec<Vec<Option<String>>> = messages
        .iter()
        .filter(|(kind, _)| *kind == b'D')
        .map(|(_, payload)| parse_row(payload))
        .collect();
    assert_eq!(
        rows,
        vec![
            vec![Some("2".to_string()), Some("b".to_string())],
            vec![Some("3".to_string()), Some("c".to_string())],
        ]
    );

    let tags: Vec<String> = messages
        .iter()
        .filter(|(kind, _)| *kind == b'C')
        .map(|(_, payload)| String::from_utf8_lossy(&payload[..payload.len() - 1]).to_string())
        .collect();
    assert_eq!(tags, vec!["SELECT 2"], "tags: {tags:?}");

    stream.write_all(b"X\x00\x00\x00\x04").expect("terminate");
}

fn ready_status(messages: &[(u8, Vec<u8>)]) -> u8 {
    messages
        .iter()
        .find(|(kind, _)| *kind == b'Z')
        .map(|(_, payload)| payload[0])
        .expect("ReadyForQuery")
}

#[test]
fn transaction_status_roundtrip() {
    let dir = TempDir::new("pgwire-tx");
    let db = dir.join("pg.db");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let db_path = db.clone();
    thread::spawn(move || {
        let _ = tritdb_pgwire::serve(db_path, listener);
    });

    let mut stream = TcpStream::connect(addr).expect("connect");
    startup(&mut stream);
    let _ = read_until_ready(&mut stream);

    query(
        &mut stream,
        "CREATE TABLE t (id INT NOT NULL); INSERT INTO t VALUES (1);",
    );
    let _ = read_until_ready(&mut stream);

    // BEGIN switches ReadyForQuery to 'T'.
    query(&mut stream, "BEGIN;");
    let messages = read_until_ready(&mut stream);
    assert_eq!(ready_status(&messages), b'T');
    assert!(messages
        .iter()
        .any(|(kind, payload)| *kind == b'C' && payload == b"BEGIN\0"));

    // A normal statement keeps the transaction open.
    query(&mut stream, "SELECT id FROM t;");
    let messages = read_until_ready(&mut stream);
    assert_eq!(ready_status(&messages), b'T');
    assert!(messages.iter().any(|(kind, _)| *kind == b'D'));

    // A failing statement marks the transaction aborted ('E').
    query(&mut stream, "SELECT * FROM missing;");
    let messages = read_until_ready(&mut stream);
    assert_eq!(ready_status(&messages), b'E');
    assert!(messages.iter().any(|(kind, _)| *kind == b'E'));

    // Further statements are rejected until the transaction ends.
    query(&mut stream, "SELECT id FROM t;");
    let messages = read_until_ready(&mut stream);
    assert_eq!(ready_status(&messages), b'E');
    assert!(messages.iter().any(|(kind, _)| *kind == b'E'));

    // ROLLBACK returns to idle.
    query(&mut stream, "ROLLBACK;");
    let messages = read_until_ready(&mut stream);
    assert_eq!(ready_status(&messages), b'I');
    assert!(messages
        .iter()
        .any(|(kind, payload)| *kind == b'C' && payload == b"ROLLBACK\0"));

    // The connection works again.
    query(&mut stream, "SELECT id FROM t;");
    let messages = read_until_ready(&mut stream);
    assert_eq!(ready_status(&messages), b'I');
    assert!(messages.iter().any(|(kind, _)| *kind == b'D'));

    stream.write_all(b"X\x00\x00\x00\x04").expect("terminate");
}

#[test]
fn describe_non_select_returns_no_data() {
    let dir = TempDir::new("pgwire-describe");
    let db = dir.join("pg.db");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let db_path = db.clone();
    thread::spawn(move || {
        let _ = tritdb_pgwire::serve(db_path, listener);
    });

    let mut stream = TcpStream::connect(addr).expect("connect");
    startup(&mut stream);
    let _ = read_until_ready(&mut stream);

    query(&mut stream, "CREATE TABLE t (id INT NOT NULL);");
    let _ = read_until_ready(&mut stream);

    write_parse(&mut stream, "", "INSERT INTO t VALUES ($1);", &[20]);
    write_describe_statement(&mut stream, "");
    write_sync(&mut stream);

    let messages = read_until_ready(&mut stream);
    let kinds: Vec<u8> = messages.iter().map(|(kind, _)| *kind).collect();
    assert!(
        kinds.contains(&b't'),
        "missing ParameterDescription: {kinds:?}"
    );
    assert!(kinds.contains(&b'n'), "missing NoData: {kinds:?}");
    assert!(
        !kinds.contains(&b'T'),
        "INSERT must not have RowDescription: {kinds:?}"
    );

    stream.write_all(b"X\x00\x00\x00\x04").expect("terminate");
}

#[test]
fn concurrent_connections_roundtrip() {
    let dir = TempDir::new("pgwire-concurrent");
    let db = dir.join("pg.db");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let db_path = db.clone();
    thread::spawn(move || {
        let _ = tritdb_pgwire::serve(db_path, listener);
    });

    let mut setup = TcpStream::connect(addr).expect("connect");
    startup(&mut setup);
    let _ = read_until_ready(&mut setup);
    query(
        &mut setup,
        "CREATE TABLE t (id INT NOT NULL); INSERT INTO t VALUES (1),(2);",
    );
    let _ = read_until_ready(&mut setup);

    let mut handles = Vec::new();
    for _ in 0..4 {
        let mut stream = TcpStream::connect(addr).expect("connect");
        handles.push(thread::spawn(move || {
            startup(&mut stream);
            let _ = read_until_ready(&mut stream);
            query(&mut stream, "SELECT id FROM t ORDER BY id;");
            let messages = read_until_ready(&mut stream);
            let rows: Vec<Vec<Option<String>>> = messages
                .iter()
                .filter(|(kind, _)| *kind == b'D')
                .map(|(_, payload)| parse_row(payload))
                .collect();
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0], vec![Some("1".to_string())]);
            assert_eq!(rows[1], vec![Some("2".to_string())]);
        }));
    }
    for handle in handles {
        handle.join().expect("join");
    }
}

#[test]
fn row_description_reports_real_type_oids() {
    let dir = TempDir::new("pgwire-oids");
    let db = dir.join("pg.db");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let db_path = db.clone();
    thread::spawn(move || {
        let _ = tritdb_pgwire::serve(db_path, listener);
    });

    let mut stream = TcpStream::connect(addr).expect("connect");
    startup(&mut stream);
    let _ = read_until_ready(&mut stream);

    query(
        &mut stream,
        "CREATE TABLE t (id INT NOT NULL, flag BOOL, name TEXT); \
         INSERT INTO t VALUES (1, TRUE, 'a'), (2, FALSE, 'b');",
    );
    let _ = read_until_ready(&mut stream);

    query(&mut stream, "SELECT id, flag, name FROM t ORDER BY id;");
    let messages = read_until_ready(&mut stream);
    let description = messages
        .iter()
        .find(|(kind, _)| *kind == b'T')
        .map(|(_, payload)| payload.clone())
        .expect("RowDescription");
    // int8(20) / bool(16) / text(25) with their fixed sizes.
    assert_eq!(
        parse_type_oids(&description),
        vec![(20, 8), (16, 1), (25, -1)]
    );

    let rows: Vec<Vec<Option<String>>> = messages
        .iter()
        .filter(|(kind, _)| *kind == b'D')
        .map(|(_, payload)| parse_row(payload))
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0],
        vec![Some("1".into()), Some("t".into()), Some("a".into())]
    );

    // NULL-only columns (no rows sampled) fall back to text.
    query(&mut stream, "SELECT id FROM t WHERE FALSE;");
    let messages = read_until_ready(&mut stream);
    let description = messages
        .iter()
        .find(|(kind, _)| *kind == b'T')
        .map(|(_, payload)| payload.clone())
        .expect("RowDescription");
    assert_eq!(parse_type_oids(&description), vec![(25, -1)]);

    stream.write_all(b"X\x00\x00\x00\x04").expect("terminate");
}

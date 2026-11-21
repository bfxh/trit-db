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

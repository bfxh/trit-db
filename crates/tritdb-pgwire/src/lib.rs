#![forbid(unsafe_code)]
//! Minimal PostgreSQL wire protocol (v3) server for trit-db.
//!
//! Supported: startup handshake, `SSLRequest` rejection, simple `Query`
//! messages, `Terminate`, and `Sync`. Extended query protocol
//! (`Parse`/`Bind`/`Execute`) is not implemented; clients must use the simple
//! query protocol (most `psql`/driver defaults do).
//!
//! Connections are handled one at a time; each connection opens its own
//! [`SqlEngine`] over the same database path.

use std::fmt;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};

use tritdb_exec::Value;
use tritdb_sql::{QueryResult, SqlEngine};

const PROTOCOL_V3: i32 = 196_608;
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

/// Serves connections until the listener is closed or fails.
///
/// Connections are accepted and handled serially; each one opens its own
/// [`SqlEngine`] on `db`.
pub fn serve(db: PathBuf, listener: TcpListener) -> Result<(), PgError> {
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("pgwire: accept failed: {error}");
                continue;
            }
        };
        if let Err(error) = handle_connection(&db, stream) {
            eprintln!("pgwire: {error}");
        }
    }
    Ok(())
}

fn handle_connection(db: &Path, mut stream: TcpStream) -> Result<(), PgError> {
    if !read_startup(&mut stream)? {
        return Ok(());
    }

    // AuthenticationOk
    write_message(&mut stream, b'R', &0i32.to_be_bytes())?;
    // A few ParameterStatus values that clients expect.
    write_parameter(&mut stream, "server_version", "14.0")?;
    write_parameter(&mut stream, "server_encoding", "UTF8")?;
    write_parameter(&mut stream, "client_encoding", "UTF8")?;
    write_parameter(&mut stream, "DateStyle", "ISO, MDY")?;
    // BackendKeyData
    let mut key = Vec::with_capacity(8);
    key.extend_from_slice(&1i32.to_be_bytes());
    key.extend_from_slice(&0i32.to_be_bytes());
    write_message(&mut stream, b'K', &key)?;
    write_message(&mut stream, b'Z', b"I")?;

    let mut engine = SqlEngine::open(db).map_err(|error| PgError::Sql(error.to_string()))?;

    loop {
        let Some((kind, payload)) = read_message(&mut stream)? else {
            return Ok(());
        };
        match kind {
            b'Q' => {
                let sql = String::from_utf8_lossy(&payload)
                    .trim_end_matches('\0')
                    .to_string();
                handle_query(&mut stream, &mut engine, &sql)?;
            }
            b'S' => write_message(&mut stream, b'Z', b"I")?,
            b'X' => return Ok(()),
            b'P' | b'B' | b'D' | b'E' | b'C' => {
                write_error(
                    &mut stream,
                    "extended query protocol is not supported; use simple queries",
                )?;
                write_message(&mut stream, b'Z', b"I")?;
            }
            other => {
                write_error(
                    &mut stream,
                    &format!("unsupported message type {}", other as char),
                )?;
                write_message(&mut stream, b'Z', b"I")?;
            }
        }
    }
}

/// Reads the startup message (handling `SSLRequest`), returning `false` when
/// the client disconnected cleanly.
fn read_startup(stream: &mut TcpStream) -> Result<bool, PgError> {
    loop {
        let mut length = [0u8; 4];
        if stream.read_exact(&mut length).is_err() {
            return Ok(false);
        }
        let length = i32::from_be_bytes(length);
        if !(8..=1_000_000).contains(&length) {
            return Err(PgError::Protocol(format!("bad startup length {length}")));
        }
        let mut body = vec![0u8; (length - 4) as usize];
        stream.read_exact(&mut body)?;
        let protocol = i32::from_be_bytes([body[0], body[1], body[2], body[3]]);
        if protocol == SSL_REQUEST {
            // Refuse TLS; the client retries with a plain startup message.
            stream.write_all(b"N")?;
            continue;
        }
        if protocol != PROTOCOL_V3 {
            return Err(PgError::Protocol(format!(
                "unsupported protocol version {protocol}"
            )));
        }
        // The rest is key\0value\0...\0; we accept and ignore parameters.
        return Ok(true);
    }
}

/// Reads one backend message, returning `None` on clean EOF.
fn read_message(stream: &mut TcpStream) -> Result<Option<(u8, Vec<u8>)>, PgError> {
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

fn write_message(stream: &mut TcpStream, kind: u8, payload: &[u8]) -> Result<(), PgError> {
    let length = (payload.len() as i32) + 4;
    stream.write_all(&[kind])?;
    stream.write_all(&length.to_be_bytes())?;
    stream.write_all(payload)?;
    Ok(())
}

fn write_parameter(stream: &mut TcpStream, key: &str, value: &str) -> Result<(), PgError> {
    let mut payload = Vec::with_capacity(key.len() + value.len() + 2);
    payload.extend_from_slice(key.as_bytes());
    payload.push(0);
    payload.extend_from_slice(value.as_bytes());
    payload.push(0);
    write_message(stream, b'S', &payload)
}

fn handle_query(stream: &mut TcpStream, engine: &mut SqlEngine, sql: &str) -> Result<(), PgError> {
    if sql.trim().is_empty() {
        write_message(stream, b'I', &[])?;
        write_message(stream, b'Z', b"I")?;
        return Ok(());
    }
    match engine.execute_script(sql) {
        Ok(results) => {
            for result in results {
                match result {
                    QueryResult::Rows { columns, rows } => {
                        write_row_description(stream, &columns)?;
                        for row in &rows {
                            write_data_row(stream, row)?;
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
            write_message(stream, b'Z', b"I")?;
            Ok(())
        }
        Err(error) => {
            write_error(stream, &error.to_string())?;
            write_message(stream, b'Z', b"I")?;
            Ok(())
        }
    }
}

fn write_row_description(stream: &mut TcpStream, columns: &[String]) -> Result<(), PgError> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&(columns.len() as i16).to_be_bytes());
    for name in columns {
        payload.extend_from_slice(name.as_bytes());
        payload.push(0);
        payload.extend_from_slice(&0i32.to_be_bytes()); // table oid
        payload.extend_from_slice(&0i16.to_be_bytes()); // attribute number
        payload.extend_from_slice(&25i32.to_be_bytes()); // TEXT oid
        payload.extend_from_slice(&(-1i16).to_be_bytes()); // type size
        payload.extend_from_slice(&(-1i32).to_be_bytes()); // type modifier
        payload.extend_from_slice(&0i16.to_be_bytes()); // text format
    }
    write_message(stream, b'T', &payload)
}

fn write_data_row(stream: &mut TcpStream, row: &[Value]) -> Result<(), PgError> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&(row.len() as i16).to_be_bytes());
    for value in row {
        if value.is_null() {
            payload.extend_from_slice(&(-1i32).to_be_bytes());
            continue;
        }
        let text = value_text(value);
        payload.extend_from_slice(&(text.len() as i32).to_be_bytes());
        payload.extend_from_slice(text.as_bytes());
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

fn write_error(stream: &mut TcpStream, message: &str) -> Result<(), PgError> {
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

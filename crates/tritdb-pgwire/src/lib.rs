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
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tritdb_exec::Value;
use tritdb_sql::{parse_script, QueryResult, SqlEngine, Statement};

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
        std::thread::spawn(move || {
            if let Err(error) = handle_connection(engine, stream) {
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

fn handle_connection(engine: Arc<Mutex<SqlEngine>>, mut stream: TcpStream) -> Result<(), PgError> {
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

    let mut prepared: BTreeMap<String, Prepared> = BTreeMap::new();
    let mut portals: BTreeMap<String, Bound> = BTreeMap::new();
    let mut tx_status = TxStatus::Idle;

    loop {
        let Some((kind, payload)) = read_message(&mut stream)? else {
            return Ok(());
        };
        match kind {
            b'Q' => {
                let sql = String::from_utf8_lossy(&payload)
                    .trim_end_matches('\0')
                    .to_string();
                let mut guard = lock_engine(&engine);
                handle_query(&mut stream, &mut guard, &sql, &mut tx_status)?;
            }
            b'S' => write_message(&mut stream, b'Z', &[tx_status.byte()])?,
            b'X' => return Ok(()),
            b'P' => parse_prepared(&mut stream, &mut prepared, &payload)?,
            b'B' => bind_portal(&mut stream, &prepared, &mut portals, &payload)?,
            b'D' => {
                let mut guard = lock_engine(&engine);
                describe(&mut stream, &mut guard, &prepared, &portals, &payload)?;
            }
            b'E' => {
                let mut guard = lock_engine(&engine);
                execute_portal(&mut stream, &mut guard, &portals, &payload, &mut tx_status)?;
            }
            b'C' => close_portal(&mut stream, &mut prepared, &mut portals, &payload)?,
            b'H' => {}
            other => {
                write_error(
                    &mut stream,
                    &format!("unsupported message type {}", other as char),
                )?;
                write_message(&mut stream, b'Z', &[tx_status.byte()])?;
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

fn handle_query(
    stream: &mut TcpStream,
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
    let success = execute_and_write(stream, engine, sql)?;
    if !success && *tx_status == TxStatus::InTransaction {
        *tx_status = TxStatus::Failed;
    }
    write_message(stream, b'Z', &[tx_status.byte()])?;
    Ok(())
}

fn execute_and_write(
    stream: &mut TcpStream,
    engine: &mut SqlEngine,
    sql: &str,
) -> Result<bool, PgError> {
    match engine.execute_script(sql) {
        Ok(results) => {
            for result in results {
                match result {
                    QueryResult::Rows { columns, rows } => {
                        let oids = column_oids(&rows, columns.len());
                        write_row_description(stream, &columns, &oids)?;
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
    stream: &mut TcpStream,
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
    stream: &mut TcpStream,
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
    portals.insert(portal, Bound { sql });
    write_message(stream, b'2', &[])
}

fn describe(
    stream: &mut TcpStream,
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
            describe_result(stream, engine, &prepared.sql, &prepared.param_types)?;
        }
        b'P' => {
            let bound = portals
                .get(&name)
                .ok_or_else(|| PgError::Protocol(format!("unknown portal {name:?}")))?;
            describe_result(stream, engine, &bound.sql, &[])?;
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
    stream: &mut TcpStream,
    engine: &mut SqlEngine,
    sql: &str,
    param_types: &[i32],
) -> Result<(), PgError> {
    match describe_columns(engine, sql, param_types) {
        Ok(Some(meta)) => write_row_description(stream, &meta.columns, &meta.oids),
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
    stream: &mut TcpStream,
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
    let success = execute_and_write(stream, engine, &bound.sql)?;
    if !success && *tx_status == TxStatus::InTransaction {
        *tx_status = TxStatus::Failed;
    }
    Ok(())
}

fn close_portal(
    stream: &mut TcpStream,
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
    stream: &mut TcpStream,
    columns: &[String],
    oids: &[i32],
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

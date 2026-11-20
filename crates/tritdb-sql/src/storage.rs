use tritdb_exec::{Column, DataType, ExecError, Schema, Table, Value};

use crate::error::SqlError;
use crate::stats::{ColumnStats, TableStats};

/// Prefix for table values in the key/value store.
pub const TABLE_KEY_PREFIX: &str = "sql/table/";
/// Prefix for `ANALYZE` statistics in the key/value store.
pub const STATS_KEY_PREFIX: &str = "sql/stats/";

const TABLE_MAGIC: [u8; 4] = *b"TRSQ";
const TABLE_VERSION: u8 = 0;
const STATS_MAGIC: [u8; 4] = *b"TRSS";
const STATS_VERSION: u8 = 0;
const MAX_COLUMNS: usize = 1024;

/// Builds the key for a table.
pub fn table_key(name: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(TABLE_KEY_PREFIX.len() + name.len());
    key.extend_from_slice(TABLE_KEY_PREFIX.as_bytes());
    key.extend_from_slice(name.as_bytes());
    key
}

/// Builds the key for table statistics.
pub fn stats_key(name: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(STATS_KEY_PREFIX.len() + name.len());
    key.extend_from_slice(STATS_KEY_PREFIX.as_bytes());
    key.extend_from_slice(name.as_bytes());
    key
}

fn put_u32(out: &mut Vec<u8>, value: usize) -> Result<(), SqlError> {
    let value = u32::try_from(value)
        .map_err(|_| SqlError::Unsupported("table field exceeds u32".to_string()))?;
    out.extend_from_slice(&value.to_le_bytes());
    Ok(())
}

fn put_i64(out: &mut Vec<u8>, value: i64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_text(out: &mut Vec<u8>, value: &str) -> Result<(), SqlError> {
    put_u32(out, value.len())?;
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

fn type_tag(data_type: DataType) -> u8 {
    match data_type {
        DataType::Int => 0,
        DataType::Text => 1,
        DataType::Bool => 2,
    }
}

fn type_from_tag(tag: u8) -> Option<DataType> {
    match tag {
        0 => Some(DataType::Int),
        1 => Some(DataType::Text),
        2 => Some(DataType::Bool),
        _ => None,
    }
}

/// Encodes a table into the value stored under the `sql/table/` key prefix.
pub fn encode_table(table: &Table) -> Result<Vec<u8>, SqlError> {
    if table.schema.columns.len() > MAX_COLUMNS {
        return Err(SqlError::Unsupported(format!(
            "table {} has more than {MAX_COLUMNS} columns",
            table.name
        )));
    }
    let mut out = Vec::new();
    out.extend_from_slice(&TABLE_MAGIC);
    out.push(TABLE_VERSION);
    put_u32(&mut out, table.schema.columns.len())?;
    for column in &table.schema.columns {
        out.push(type_tag(column.data_type));
        out.push(u8::from(column.not_null));
        put_text(&mut out, &column.name)?;
    }
    out.extend_from_slice(&(table.rows.len() as u64).to_le_bytes());
    for row in &table.rows {
        if row.len() != table.schema.columns.len() {
            return Err(SqlError::Exec(ExecError::RowShape {
                expected: table.schema.columns.len(),
                actual: row.len(),
            }));
        }
        for (value, column) in row.iter().zip(&table.schema.columns) {
            encode_value(&mut out, value, column.data_type, &table.name, &column.name)?;
        }
    }
    Ok(out)
}

/// Encodes exact table statistics into the value stored under the
/// `sql/stats/` key prefix.
pub fn encode_stats(stats: &TableStats) -> Result<Vec<u8>, SqlError> {
    if stats.columns.len() > MAX_COLUMNS {
        return Err(SqlError::Unsupported(format!(
            "statistics have more than {MAX_COLUMNS} columns"
        )));
    }
    let mut out = Vec::new();
    out.extend_from_slice(&STATS_MAGIC);
    out.push(STATS_VERSION);
    put_u64(&mut out, stats.rows);
    put_u32(&mut out, stats.columns.len())?;
    for column in &stats.columns {
        put_text(&mut out, &column.name)?;
        put_u64(&mut out, column.nulls);
        put_u64(&mut out, column.distinct);
    }
    Ok(out)
}

fn encode_value(
    out: &mut Vec<u8>,
    value: &Value,
    data_type: DataType,
    table: &str,
    column: &str,
) -> Result<(), SqlError> {
    match value {
        Value::Null => {
            out.push(0);
            Ok(())
        }
        Value::Int(value) if data_type == DataType::Int => {
            out.push(1);
            put_i64(out, *value);
            Ok(())
        }
        Value::Text(value) if data_type == DataType::Text => {
            out.push(2);
            put_text(out, value)
        }
        Value::Bool(value) if data_type == DataType::Bool => {
            out.push(3);
            out.push(u8::from(*value));
            Ok(())
        }
        other => Err(SqlError::CorruptTable {
            table: table.to_string(),
            reason: format!(
                "column {column} expects {}, got {}",
                data_type.name(),
                other.type_name()
            ),
        }),
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn u8(&mut self) -> Option<u8> {
        let value = *self.bytes.get(self.pos)?;
        self.pos += 1;
        Some(value)
    }

    fn u32(&mut self) -> Option<u32> {
        let bytes = self.bytes.get(self.pos..self.pos + 4)?;
        self.pos += 4;
        Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self) -> Option<u64> {
        let bytes = self.bytes.get(self.pos..self.pos + 8)?;
        self.pos += 8;
        Some(u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    fn text(&mut self, len: usize) -> Option<String> {
        let bytes = self.bytes.get(self.pos..self.pos + len)?;
        self.pos += len;
        std::str::from_utf8(bytes).ok().map(str::to_string)
    }

    fn i64(&mut self) -> Option<i64> {
        let bytes = self.bytes.get(self.pos..self.pos + 8)?;
        self.pos += 8;
        Some(i64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }
}

/// Decodes a table value, checking that it belongs to `name`.
pub fn decode_table(name: &str, bytes: &[u8]) -> Result<Table, SqlError> {
    let corrupt = |reason: &str| SqlError::CorruptTable {
        table: name.to_string(),
        reason: reason.to_string(),
    };
    let mut reader = Reader::new(bytes);
    if reader.bytes.get(0..4) != Some(TABLE_MAGIC.as_slice()) {
        return Err(corrupt("bad magic"));
    }
    reader.pos = 4;
    let version = reader.u8().ok_or_else(|| corrupt("missing version"))?;
    if version != TABLE_VERSION {
        return Err(corrupt("unsupported version"));
    }
    let column_count = reader
        .u32()
        .ok_or_else(|| corrupt("missing column count"))? as usize;
    if column_count == 0 || column_count > MAX_COLUMNS {
        return Err(corrupt("invalid column count"));
    }
    let mut columns = Vec::with_capacity(column_count);
    for _ in 0..column_count {
        let type_tag = reader.u8().ok_or_else(|| corrupt("missing column type"))?;
        let data_type = type_from_tag(type_tag).ok_or_else(|| corrupt("invalid column type"))?;
        let not_null = reader.u8().ok_or_else(|| corrupt("missing null flag"))? != 0;
        let name_len = reader
            .u32()
            .ok_or_else(|| corrupt("missing column name length"))? as usize;
        let column_name = reader
            .text(name_len)
            .ok_or_else(|| corrupt("invalid column name"))?;
        columns.push(Column {
            name: column_name,
            data_type,
            not_null,
        });
    }
    let row_count = reader.u64().ok_or_else(|| corrupt("missing row count"))?;
    let mut rows = Vec::new();
    for _ in 0..row_count {
        let mut row = Vec::with_capacity(column_count);
        for column in &columns {
            let tag = reader.u8().ok_or_else(|| corrupt("missing value tag"))?;
            let value = match tag {
                0 => Value::Null,
                1 => Value::Int(reader.i64().ok_or_else(|| corrupt("truncated int"))?),
                2 => {
                    let len = reader.u32().ok_or_else(|| corrupt("missing text length"))? as usize;
                    Value::Text(reader.text(len).ok_or_else(|| corrupt("invalid text"))?)
                }
                3 => Value::Bool(reader.u8().ok_or_else(|| corrupt("missing bool"))? != 0),
                _ => return Err(corrupt("invalid value tag")),
            };
            if !value.is_null() && value.type_name() != column.data_type.name() {
                return Err(corrupt("value type does not match column"));
            }
            if value.is_null() && column.not_null {
                return Err(corrupt("NULL in NOT NULL column"));
            }
            row.push(value);
        }
        rows.push(row);
    }
    if reader.pos != bytes.len() {
        return Err(corrupt("trailing bytes"));
    }
    Ok(Table {
        name: name.to_string(),
        schema: Schema::new(columns),
        rows,
    })
}

/// Decodes table statistics, checking that they still match the table schema
/// and row count.
pub fn decode_stats(table: &Table, bytes: &[u8]) -> Result<TableStats, SqlError> {
    let corrupt = |reason: &str| SqlError::CorruptStats {
        table: table.name.clone(),
        reason: reason.to_string(),
    };
    let mut reader = Reader::new(bytes);
    if reader.bytes.get(0..4) != Some(STATS_MAGIC.as_slice()) {
        return Err(corrupt("bad magic"));
    }
    reader.pos = 4;
    let version = reader.u8().ok_or_else(|| corrupt("missing version"))?;
    if version != STATS_VERSION {
        return Err(corrupt("unsupported version"));
    }
    let rows = reader.u64().ok_or_else(|| corrupt("missing row count"))?;
    let column_count = reader
        .u32()
        .ok_or_else(|| corrupt("missing column count"))? as usize;
    if column_count == 0 || column_count > MAX_COLUMNS {
        return Err(corrupt("invalid column count"));
    }
    if column_count != table.schema.len() {
        return Err(corrupt("column count does not match table"));
    }
    let mut columns = Vec::with_capacity(column_count);
    for schema_column in &table.schema.columns {
        let name_len = reader
            .u32()
            .ok_or_else(|| corrupt("missing column name length"))? as usize;
        let name = reader
            .text(name_len)
            .ok_or_else(|| corrupt("invalid column name"))?;
        if name != schema_column.name {
            return Err(corrupt("column name does not match table"));
        }
        let nulls = reader.u64().ok_or_else(|| corrupt("missing null count"))?;
        let distinct = reader
            .u64()
            .ok_or_else(|| corrupt("missing distinct count"))?;
        if nulls > rows {
            return Err(corrupt("null count exceeds row count"));
        }
        if distinct > rows - nulls {
            return Err(corrupt("distinct count exceeds non-null row count"));
        }
        columns.push(ColumnStats {
            name,
            nulls,
            distinct,
        });
    }
    if reader.pos != bytes.len() {
        return Err(corrupt("trailing bytes"));
    }
    if rows != table.rows.len() as u64 {
        return Err(corrupt("row count does not match table"));
    }
    Ok(TableStats { rows, columns })
}

#[cfg(test)]
mod tests {
    use crate::stats::analyze_table;

    use super::*;

    fn table() -> Table {
        Table {
            name: "t".to_string(),
            schema: Schema::new(vec![
                Column::new("id", DataType::Int),
                Column {
                    name: "name".to_string(),
                    data_type: DataType::Text,
                    not_null: true,
                },
                Column::new("ok", DataType::Bool),
            ]),
            rows: vec![
                vec![
                    Value::Int(1),
                    Value::Text("a".to_string()),
                    Value::Bool(true),
                ],
                vec![Value::Null, Value::Text("b".to_string()), Value::Null],
            ],
        }
    }

    #[test]
    fn table_roundtrip() {
        let table = table();
        let bytes = encode_table(&table).expect("encode");
        let decoded = decode_table("t", &bytes).expect("decode");
        assert_eq!(decoded, table);
    }

    #[test]
    fn rejects_truncated_table() {
        let bytes = encode_table(&table()).expect("encode");
        for end in 0..bytes.len() {
            assert!(decode_table("t", &bytes[..end]).is_err(), "prefix {end}");
        }
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut bytes = encode_table(&table()).expect("encode");
        bytes.push(0);
        assert!(decode_table("t", &bytes).is_err());
    }

    #[test]
    fn rejects_type_mismatch() {
        let mut table = table();
        table.rows[0][0] = Value::Text("x".to_string());
        assert!(encode_table(&table).is_err());
    }

    #[test]
    fn stats_roundtrip_and_validate_table() {
        let table = table();
        let stats = analyze_table(&table);
        let bytes = encode_stats(&stats).expect("encode stats");
        let decoded = decode_stats(&table, &bytes).expect("decode stats");
        assert_eq!(decoded, stats);

        for end in 0..bytes.len() {
            assert!(
                decode_stats(&table, &bytes[..end]).is_err(),
                "stats prefix {end}"
            );
        }

        let mut shorter = table;
        shorter.rows.pop();
        assert!(decode_stats(&shorter, &bytes).is_err());
    }
}

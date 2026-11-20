use std::error::Error;
use std::fmt;

use tritdb_exec::ExecError;
use tritdb_txn::TxnError;

/// SQL front-end error.
#[derive(Debug)]
pub enum SqlError {
    /// Lexer or parser error.
    Parse {
        /// Human-readable message.
        message: String,
        /// Character offset in the input.
        position: usize,
    },
    /// Table does not exist.
    UnknownTable(String),
    /// Table already exists.
    TableExists(String),
    /// Column does not exist in the table.
    UnknownColumn(String),
    /// Unqualified column name matches more than one joined column.
    AmbiguousColumn(String),
    /// Duplicate table name or alias in a `FROM` clause.
    DuplicateTableAlias(String),
    /// Duplicate column in `CREATE TABLE`.
    DuplicateColumn(String),
    /// `INSERT` value count does not match the target column count.
    ColumnCount {
        /// Table name.
        table: String,
        /// Expected count.
        expected: usize,
        /// Actual count.
        actual: usize,
    },
    /// `NOT NULL` column received `NULL`.
    NotNull {
        /// Table name.
        table: String,
        /// Column name.
        column: String,
    },
    /// A `CHECK` constraint evaluated to `FALSE`.
    CheckViolation {
        /// Table name.
        table: String,
        /// Constraint text.
        expression: String,
    },
    /// A `UNIQUE` column received a duplicate non-`NULL` value.
    DuplicateKey {
        /// Table name.
        table: String,
        /// Column name.
        column: String,
        /// Duplicated value in display form.
        value: String,
    },
    /// Aggregate expression is nested inside another aggregate.
    NestedAggregate,
    /// A column reference appears outside an aggregate in an aggregate query.
    ColumnOutsideAggregate(String),
    /// A column reference is not part of the `GROUP BY` key.
    ColumnOutsideGroup(String),
    /// Unsupported SQL feature.
    Unsupported(String),
    /// Persisted table value is corrupt.
    CorruptTable {
        /// Table name.
        table: String,
        /// Human-readable reason.
        reason: String,
    },
    /// Persisted table statistics are corrupt or do not match the table.
    CorruptStats {
        /// Table name.
        table: String,
        /// Human-readable reason.
        reason: String,
    },
    /// Expression evaluation error.
    Exec(ExecError),
    /// Transaction/WAL error.
    Txn(TxnError),
    /// I/O error.
    Io(std::io::Error),
}

impl SqlError {
    pub(crate) fn parse(message: impl Into<String>, position: usize) -> Self {
        Self::Parse {
            message: message.into(),
            position,
        }
    }
}

impl fmt::Display for SqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SqlError::Parse { message, position } => {
                write!(f, "SQL parse error at character {position}: {message}")
            }
            SqlError::UnknownTable(name) => write!(f, "unknown table: {name}"),
            SqlError::TableExists(name) => write!(f, "table already exists: {name}"),
            SqlError::UnknownColumn(name) => write!(f, "unknown column: {name}"),
            SqlError::AmbiguousColumn(name) => write!(f, "ambiguous column: {name}"),
            SqlError::DuplicateTableAlias(name) => {
                write!(f, "duplicate table name or alias: {name}")
            }
            SqlError::DuplicateColumn(name) => write!(f, "duplicate column: {name}"),
            SqlError::ColumnCount {
                table,
                expected,
                actual,
            } => write!(
                f,
                "INSERT into {table} expects {expected} values, got {actual}"
            ),
            SqlError::NotNull { table, column } => {
                write!(f, "{table}.{column} is NOT NULL")
            }
            SqlError::DuplicateKey {
                table,
                column,
                value,
            } => write!(f, "{table}.{column} UNIQUE violated by {value}"),
            SqlError::CheckViolation { table, expression } => {
                write!(f, "{table} CHECK violated: {expression}")
            }
            SqlError::NestedAggregate => write!(f, "nested aggregate is not supported"),
            SqlError::ColumnOutsideAggregate(name) => {
                write!(f, "column {name} must appear inside an aggregate")
            }
            SqlError::ColumnOutsideGroup(name) => {
                write!(f, "column {name} must appear in GROUP BY")
            }
            SqlError::Unsupported(feature) => write!(f, "unsupported SQL: {feature}"),
            SqlError::CorruptTable { table, reason } => {
                write!(f, "corrupt table {table}: {reason}")
            }
            SqlError::CorruptStats { table, reason } => {
                write!(f, "corrupt statistics for table {table}: {reason}")
            }
            SqlError::Exec(error) => write!(f, "SQL evaluation error: {error}"),
            SqlError::Txn(error) => write!(f, "SQL transaction error: {error}"),
            SqlError::Io(error) => write!(f, "SQL I/O error: {error}"),
        }
    }
}

impl Error for SqlError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            SqlError::Exec(error) => Some(error),
            SqlError::Txn(error) => Some(error),
            SqlError::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ExecError> for SqlError {
    fn from(error: ExecError) -> Self {
        SqlError::Exec(error)
    }
}

impl From<TxnError> for SqlError {
    fn from(error: TxnError) -> Self {
        SqlError::Txn(error)
    }
}

impl From<std::io::Error> for SqlError {
    fn from(error: std::io::Error) -> Self {
        SqlError::Io(error)
    }
}

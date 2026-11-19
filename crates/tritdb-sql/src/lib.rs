#![forbid(unsafe_code)]
//! Minimal SQL front end over [`tritdb_txn::TxnStore`].

mod ast;
mod bind;
mod engine;
mod error;
mod lexer;
mod parser;
mod storage;

pub use ast::{AggregateKind, ColumnDef, Expr, OrderBy, Select, SelectItem, Statement};
pub use engine::{QueryResult, SqlEngine};
pub use error::SqlError;
pub use parser::parse_script;
pub use storage::{decode_table, encode_table, TABLE_KEY_PREFIX};

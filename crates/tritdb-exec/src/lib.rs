#![forbid(unsafe_code)]
//! SQL value model and three-valued expression evaluation.
//!
//! `NULL` is `UNKNOWN` in boolean context. Predicates return
//! [`TruthValue`] internally and are materialized as `TRUE`/`FALSE`/`NULL`
//! when they appear as scalar values.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

/// A SQL scalar value.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Value {
    /// SQL `NULL`, also boolean `UNKNOWN`.
    Null,
    /// Signed 64-bit integer.
    Int(i64),
    /// UTF-8 text.
    Text(String),
    /// Boolean.
    Bool(bool),
}

impl Value {
    /// SQL type name used in diagnostics.
    pub const fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "NULL",
            Value::Int(_) => "INT",
            Value::Text(_) => "TEXT",
            Value::Bool(_) => "BOOL",
        }
    }

    /// Returns `true` for `NULL`.
    pub const fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Stable display form, used by the CLI and tests.
    pub fn display(&self) -> String {
        match self {
            Value::Null => "NULL".to_string(),
            Value::Int(value) => value.to_string(),
            Value::Text(value) => value.clone(),
            Value::Bool(true) => "TRUE".to_string(),
            Value::Bool(false) => "FALSE".to_string(),
        }
    }
}

/// Column type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataType {
    /// 64-bit integer.
    Int,
    /// UTF-8 text.
    Text,
    /// Boolean.
    Bool,
}

impl DataType {
    /// Parses a SQL type name.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "INT" | "INTEGER" | "BIGINT" => Some(DataType::Int),
            "TEXT" | "VARCHAR" | "STRING" => Some(DataType::Text),
            "BOOL" | "BOOLEAN" => Some(DataType::Bool),
            _ => None,
        }
    }

    /// Canonical type name.
    pub const fn name(self) -> &'static str {
        match self {
            DataType::Int => "INT",
            DataType::Text => "TEXT",
            DataType::Bool => "BOOL",
        }
    }

    /// Coerces a value into this column type, rejecting `NULL` only when the
    /// caller separately enforces `NOT NULL`.
    pub fn coerce(self, value: Value) -> Result<Value, ExecError> {
        match (self, value) {
            (_, Value::Null) => Ok(Value::Null),
            (DataType::Int, Value::Int(value)) => Ok(Value::Int(value)),
            (DataType::Text, Value::Text(value)) => Ok(Value::Text(value)),
            (DataType::Bool, Value::Bool(value)) => Ok(Value::Bool(value)),
            (expected, actual) => Err(ExecError::TypeMismatch {
                expected: expected.name(),
                actual: actual.type_name(),
            }),
        }
    }
}

/// A table column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Column {
    /// Column name. Unquoted SQL identifiers are normalized to lowercase.
    pub name: String,
    /// Column type.
    pub data_type: DataType,
    /// Reject `NULL` on insert.
    pub not_null: bool,
}

impl Column {
    /// Creates a nullable column.
    pub fn new(name: impl Into<String>, data_type: DataType) -> Self {
        Self {
            name: name.into(),
            data_type,
            not_null: false,
        }
    }
}

/// Table schema.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Schema {
    /// Ordered columns.
    pub columns: Vec<Column>,
    /// Additional qualified names that resolve to an existing column. `USING`
    /// and `NATURAL JOIN` expose one merged output column through both input
    /// qualifiers, while `SELECT *` must still see it only once.
    aliases: BTreeMap<String, usize>,
}

impl Schema {
    /// Creates a schema from columns.
    pub fn new(columns: Vec<Column>) -> Self {
        Self {
            columns,
            aliases: BTreeMap::new(),
        }
    }

    /// Adds qualified aliases that resolve to existing column indexes.
    pub fn with_aliases(mut self, aliases: BTreeMap<String, usize>) -> Self {
        self.aliases = aliases;
        self
    }

    /// Returns the alias map.
    pub fn aliases(&self) -> &BTreeMap<String, usize> {
        &self.aliases
    }

    /// Number of columns.
    pub fn len(&self) -> usize {
        self.columns.len()
    }

    /// Returns `true` when there are no columns.
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// Resolves a column name exactly, then through a join alias.
    /// The parser lowercases unquoted identifiers and preserves quoted
    /// identifiers.
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|column| column.name == name)
            .or_else(|| self.aliases.get(name).copied())
    }
}

/// One row, ordered by schema.
pub type Row = Vec<Value>;

/// A table held by the SQL engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Table {
    /// Table name.
    pub name: String,
    /// Schema.
    pub schema: Schema,
    /// Rows.
    pub rows: Vec<Row>,
}

/// SQL three-valued logic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TruthValue {
    /// True.
    True,
    /// False.
    False,
    /// Unknown (`NULL`).
    Unknown,
}

impl TruthValue {
    /// SQL `NOT`.
    pub const fn not(self) -> Self {
        match self {
            TruthValue::True => TruthValue::False,
            TruthValue::False => TruthValue::True,
            TruthValue::Unknown => TruthValue::Unknown,
        }
    }

    /// SQL `AND`.
    pub const fn and(self, other: Self) -> Self {
        match (self, other) {
            (TruthValue::False, _) | (_, TruthValue::False) => TruthValue::False,
            (TruthValue::True, TruthValue::True) => TruthValue::True,
            _ => TruthValue::Unknown,
        }
    }

    /// SQL `OR`.
    pub const fn or(self, other: Self) -> Self {
        match (self, other) {
            (TruthValue::True, _) | (_, TruthValue::True) => TruthValue::True,
            (TruthValue::False, TruthValue::False) => TruthValue::False,
            _ => TruthValue::Unknown,
        }
    }

    /// Materializes the boolean as a scalar: `UNKNOWN` becomes `NULL`.
    pub const fn to_value(self) -> Value {
        match self {
            TruthValue::True => Value::Bool(true),
            TruthValue::False => Value::Bool(false),
            TruthValue::Unknown => Value::Null,
        }
    }
}

/// Comparison operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    /// `=`.
    Eq,
    /// `<>` or `!=`.
    Ne,
    /// `<`.
    Lt,
    /// `<=`.
    Le,
    /// `>`.
    Gt,
    /// `>=`.
    Ge,
}

/// Arithmetic operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithOp {
    /// `+`.
    Add,
    /// `-`.
    Sub,
    /// `*`.
    Mul,
    /// `/`.
    Div,
}

/// A bound expression. Column references are indexes into the row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expr {
    /// Literal value.
    Literal(Value),
    /// Column index.
    Column(usize),
    /// `expr IS [NOT] NULL`.
    IsNull {
        /// Input expression.
        expr: Box<Expr>,
        /// `true` for `IS NOT NULL`.
        negated: bool,
    },
    /// `expr IS [NOT] TRUE|FALSE|UNKNOWN`.
    IsTruth {
        /// Input expression.
        expr: Box<Expr>,
        /// Expected truth value.
        expected: TruthValue,
        /// `true` for `IS NOT`.
        negated: bool,
    },
    /// `NOT expr`.
    Not(Box<Expr>),
    /// `expr [NOT] IN (value, ...)`.
    InList {
        /// Probe expression.
        expr: Box<Expr>,
        /// Candidate values.
        values: Vec<Expr>,
        /// `true` for `NOT IN`.
        negated: bool,
    },
    /// `expr [NOT] BETWEEN low AND high`.
    Between {
        /// Probe expression.
        expr: Box<Expr>,
        /// Lower bound.
        low: Box<Expr>,
        /// Upper bound.
        high: Box<Expr>,
        /// `true` for `NOT BETWEEN`.
        negated: bool,
    },
    /// `expr [NOT] LIKE pattern`.
    Like {
        /// Probe expression.
        expr: Box<Expr>,
        /// Pattern expression.
        pattern: Box<Expr>,
        /// `true` for `NOT LIKE`.
        negated: bool,
    },
    /// `CASE [operand] WHEN ... THEN ... [ELSE ...] END`.
    Case {
        /// Simple-case operand; `None` for a searched case.
        operand: Option<Box<Expr>>,
        /// `WHEN`/`THEN` pairs in source order.
        branches: Vec<(Expr, Expr)>,
        /// `ELSE` result; `None` yields `NULL` when nothing matches.
        else_expr: Option<Box<Expr>>,
    },
    /// Binary boolean, comparison, or arithmetic operator.
    Binary {
        /// Operator.
        op: BinaryOp,
        /// Left operand.
        left: Box<Expr>,
        /// Right operand.
        right: Box<Expr>,
    },
}

/// Binary operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    /// `AND`.
    And,
    /// `OR`.
    Or,
    /// `=`.
    Eq,
    /// `<>`.
    Ne,
    /// `<`.
    Lt,
    /// `<=`.
    Le,
    /// `>`.
    Gt,
    /// `>=`.
    Ge,
    /// `+`.
    Add,
    /// `-`.
    Sub,
    /// `*`.
    Mul,
    /// `/`.
    Div,
}

impl BinaryOp {
    /// Comparison operator, if this is one.
    pub const fn cmp(self) -> Option<CmpOp> {
        match self {
            BinaryOp::Eq => Some(CmpOp::Eq),
            BinaryOp::Ne => Some(CmpOp::Ne),
            BinaryOp::Lt => Some(CmpOp::Lt),
            BinaryOp::Le => Some(CmpOp::Le),
            BinaryOp::Gt => Some(CmpOp::Gt),
            BinaryOp::Ge => Some(CmpOp::Ge),
            _ => None,
        }
    }

    /// Arithmetic operator, if this is one.
    pub const fn arith(self) -> Option<ArithOp> {
        match self {
            BinaryOp::Add => Some(ArithOp::Add),
            BinaryOp::Sub => Some(ArithOp::Sub),
            BinaryOp::Mul => Some(ArithOp::Mul),
            BinaryOp::Div => Some(ArithOp::Div),
            _ => None,
        }
    }
}

/// Structured expression/execution error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecError {
    /// A bound column index is outside the row.
    RowShape {
        /// Schema column count.
        expected: usize,
        /// Actual row length.
        actual: usize,
    },
    /// A column index is outside the schema.
    ColumnIndex {
        /// Index.
        index: usize,
        /// Schema column count.
        columns: usize,
    },
    /// Type mismatch.
    TypeMismatch {
        /// Expected type.
        expected: &'static str,
        /// Actual type.
        actual: &'static str,
    },
    /// Division by zero.
    DivisionByZero,
    /// `i64` overflow.
    IntegerOverflow,
    /// A predicate expression produced a non-boolean value.
    NonBooleanPredicate {
        /// Actual type.
        actual: &'static str,
    },
}

impl fmt::Display for ExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExecError::RowShape { expected, actual } => {
                write!(
                    f,
                    "row shape mismatch: expected {expected} values, got {actual}"
                )
            }
            ExecError::ColumnIndex { index, columns } => {
                write!(f, "column index {index} out of range for {columns} columns")
            }
            ExecError::TypeMismatch { expected, actual } => {
                write!(f, "type mismatch: expected {expected}, got {actual}")
            }
            ExecError::DivisionByZero => write!(f, "division by zero"),
            ExecError::IntegerOverflow => write!(f, "integer overflow"),
            ExecError::NonBooleanPredicate { actual } => {
                write!(f, "predicate must be BOOLEAN, got {actual}")
            }
        }
    }
}

impl Error for ExecError {}

/// Converts a scalar to a truth value. `NULL` is `UNKNOWN`; non-boolean
/// scalars are rejected in predicate context.
pub fn truth_of(value: &Value) -> Result<TruthValue, ExecError> {
    match value {
        Value::Bool(true) => Ok(TruthValue::True),
        Value::Bool(false) => Ok(TruthValue::False),
        Value::Null => Ok(TruthValue::Unknown),
        other => Err(ExecError::NonBooleanPredicate {
            actual: other.type_name(),
        }),
    }
}

/// Compares two values. `NULL` yields `None`; mixed non-null types are an
/// error rather than an implicit cast.
pub fn compare_values(left: &Value, right: &Value) -> Result<Option<Ordering>, ExecError> {
    match (left, right) {
        (Value::Null, _) | (_, Value::Null) => Ok(None),
        (Value::Int(left), Value::Int(right)) => Ok(Some(left.cmp(right))),
        (Value::Text(left), Value::Text(right)) => Ok(Some(left.cmp(right))),
        (Value::Bool(left), Value::Bool(right)) => Ok(Some(left.cmp(right))),
        (left, right) => Err(ExecError::TypeMismatch {
            expected: left.type_name(),
            actual: right.type_name(),
        }),
    }
}

fn compare_op(op: CmpOp, ordering: Ordering) -> bool {
    match op {
        CmpOp::Eq => ordering == Ordering::Equal,
        CmpOp::Ne => ordering != Ordering::Equal,
        CmpOp::Lt => ordering == Ordering::Less,
        CmpOp::Le => ordering != Ordering::Greater,
        CmpOp::Gt => ordering == Ordering::Greater,
        CmpOp::Ge => ordering != Ordering::Less,
    }
}

fn comparison_truth(op: CmpOp, ordering: Option<Ordering>) -> TruthValue {
    match ordering {
        None => TruthValue::Unknown,
        Some(ordering) => {
            if compare_op(op, ordering) {
                TruthValue::True
            } else {
                TruthValue::False
            }
        }
    }
}

/// SQLite-style `LIKE`: `%` matches any sequence, `_` matches one character,
/// and ASCII letters compare case-insensitively.
fn like_matches(text: &str, pattern: &str) -> bool {
    let text: Vec<char> = text.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    let mut table = vec![vec![false; pattern.len() + 1]; text.len() + 1];
    table[0][0] = true;
    for column in 1..=pattern.len() {
        table[0][column] = pattern[column - 1] == '%' && table[0][column - 1];
    }
    for row in 1..=text.len() {
        for column in 1..=pattern.len() {
            table[row][column] = match pattern[column - 1] {
                '%' => table[row][column - 1] || table[row - 1][column],
                '_' => table[row - 1][column - 1],
                expected => {
                    table[row - 1][column - 1] && expected.eq_ignore_ascii_case(&text[row - 1])
                }
            };
        }
    }
    table[text.len()][pattern.len()]
}

fn eval_arith(op: ArithOp, left: i64, right: i64) -> Result<Value, ExecError> {
    let value = match op {
        ArithOp::Add => left.checked_add(right),
        ArithOp::Sub => left.checked_sub(right),
        ArithOp::Mul => left.checked_mul(right),
        ArithOp::Div => {
            if right == 0 {
                return Err(ExecError::DivisionByZero);
            }
            left.checked_div(right)
        }
    };
    value.map(Value::Int).ok_or(ExecError::IntegerOverflow)
}

/// Evaluates a bound expression against a schema and row.
pub fn eval(expr: &Expr, schema: &Schema, row: &[Value]) -> Result<Value, ExecError> {
    if row.len() != schema.len() {
        return Err(ExecError::RowShape {
            expected: schema.len(),
            actual: row.len(),
        });
    }
    match expr {
        Expr::Literal(value) => Ok(value.clone()),
        Expr::Column(index) => {
            if *index >= schema.len() {
                return Err(ExecError::ColumnIndex {
                    index: *index,
                    columns: schema.len(),
                });
            }
            Ok(row[*index].clone())
        }
        Expr::IsNull { expr, negated } => {
            let value = eval(expr, schema, row)?;
            let is_null = value.is_null();
            Ok(Value::Bool(if *negated { !is_null } else { is_null }))
        }
        Expr::IsTruth {
            expr,
            expected,
            negated,
        } => {
            let value = eval(expr, schema, row)?;
            let truth = truth_of(&value)?;
            let matched = truth == *expected;
            Ok(Value::Bool(if *negated { !matched } else { matched }))
        }
        Expr::Not(expr) => {
            let value = eval(expr, schema, row)?;
            Ok(truth_of(&value)?.not().to_value())
        }
        Expr::InList {
            expr,
            values,
            negated,
        } => {
            let probe = eval(expr, schema, row)?;
            let mut unknown = probe.is_null();
            let mut matched = false;
            for value in values {
                match compare_values(&probe, &eval(value, schema, row)?)? {
                    Some(Ordering::Equal) => {
                        matched = true;
                        break;
                    }
                    Some(_) => {}
                    None => unknown = true,
                }
            }
            let truth = if matched {
                TruthValue::True
            } else if unknown {
                TruthValue::Unknown
            } else {
                TruthValue::False
            };
            Ok(if *negated {
                truth.not().to_value()
            } else {
                truth.to_value()
            })
        }
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => {
            let value = eval(expr, schema, row)?;
            let lower =
                comparison_truth(CmpOp::Ge, compare_values(&value, &eval(low, schema, row)?)?);
            let upper = comparison_truth(
                CmpOp::Le,
                compare_values(&value, &eval(high, schema, row)?)?,
            );
            let truth = lower.and(upper);
            Ok(if *negated {
                truth.not().to_value()
            } else {
                truth.to_value()
            })
        }
        Expr::Like {
            expr,
            pattern,
            negated,
        } => {
            let value = eval(expr, schema, row)?;
            let pattern = eval(pattern, schema, row)?;
            match (&value, &pattern) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Text(text), Value::Text(pattern)) => {
                    let matched = like_matches(text, pattern);
                    Ok(Value::Bool(if *negated { !matched } else { matched }))
                }
                (value, _) => Err(ExecError::TypeMismatch {
                    expected: "TEXT",
                    actual: value.type_name(),
                }),
            }
        }
        Expr::Case {
            operand,
            branches,
            else_expr,
        } => {
            if let Some(operand) = operand {
                let probe = eval(operand, schema, row)?;
                for (when, then) in branches {
                    let candidate = eval(when, schema, row)?;
                    if compare_values(&probe, &candidate)? == Some(Ordering::Equal) {
                        return eval(then, schema, row);
                    }
                }
            } else {
                for (when, then) in branches {
                    let value = eval(when, schema, row)?;
                    if truth_of(&value)? == TruthValue::True {
                        return eval(then, schema, row);
                    }
                }
            }
            match else_expr {
                Some(expr) => eval(expr, schema, row),
                None => Ok(Value::Null),
            }
        }
        Expr::Binary { op, left, right } => {
            if matches!(op, BinaryOp::And | BinaryOp::Or) {
                let left_value = eval(left, schema, row)?;
                let right_value = eval(right, schema, row)?;
                let left_truth = truth_of(&left_value)?;
                let right_truth = truth_of(&right_value)?;
                let result = if matches!(op, BinaryOp::And) {
                    left_truth.and(right_truth)
                } else {
                    left_truth.or(right_truth)
                };
                return Ok(result.to_value());
            }

            let left_value = eval(left, schema, row)?;
            let right_value = eval(right, schema, row)?;

            if let Some(cmp) = op.cmp() {
                return match compare_values(&left_value, &right_value)? {
                    None => Ok(Value::Null),
                    Some(ordering) => Ok(Value::Bool(compare_op(cmp, ordering))),
                };
            }

            if let Some(arith) = op.arith() {
                if left_value.is_null() || right_value.is_null() {
                    return Ok(Value::Null);
                }
                return match (left_value, right_value) {
                    (Value::Int(left), Value::Int(right)) => eval_arith(arith, left, right),
                    (left, right) => Err(ExecError::TypeMismatch {
                        expected: left.type_name(),
                        actual: right.type_name(),
                    }),
                };
            }

            unreachable!("all binary operators handled")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn truth(op: BinaryOp, left: TruthValue, right: TruthValue) -> TruthValue {
        let schema = Schema::new(vec![
            Column::new("a", DataType::Bool),
            Column::new("b", DataType::Bool),
        ]);
        let row = vec![left.to_value(), right.to_value()];
        let expr = Expr::Binary {
            op,
            left: Box::new(Expr::Column(0)),
            right: Box::new(Expr::Column(1)),
        };
        let value = eval(&expr, &schema, &row).expect("eval");
        truth_of(&value).expect("truth")
    }

    fn all() -> [TruthValue; 3] {
        [TruthValue::True, TruthValue::False, TruthValue::Unknown]
    }

    #[test]
    fn not_matches_sql_truth_table() {
        assert_eq!(TruthValue::True.not(), TruthValue::False);
        assert_eq!(TruthValue::False.not(), TruthValue::True);
        assert_eq!(TruthValue::Unknown.not(), TruthValue::Unknown);
    }

    #[test]
    fn and_matches_sql_truth_table() {
        for left in all() {
            for right in all() {
                let expected = match (left, right) {
                    (TruthValue::False, _) | (_, TruthValue::False) => TruthValue::False,
                    (TruthValue::True, TruthValue::True) => TruthValue::True,
                    _ => TruthValue::Unknown,
                };
                assert_eq!(
                    truth(BinaryOp::And, left, right),
                    expected,
                    "{left:?} AND {right:?}"
                );
            }
        }
    }

    #[test]
    fn or_matches_sql_truth_table() {
        for left in all() {
            for right in all() {
                let expected = match (left, right) {
                    (TruthValue::True, _) | (_, TruthValue::True) => TruthValue::True,
                    (TruthValue::False, TruthValue::False) => TruthValue::False,
                    _ => TruthValue::Unknown,
                };
                assert_eq!(
                    truth(BinaryOp::Or, left, right),
                    expected,
                    "{left:?} OR {right:?}"
                );
            }
        }
    }

    #[test]
    fn null_comparison_is_unknown() {
        assert_eq!(
            compare_values(&Value::Null, &Value::Int(1)).expect("cmp"),
            None
        );
        assert_eq!(
            compare_values(&Value::Int(1), &Value::Null).expect("cmp"),
            None
        );
    }

    #[test]
    fn comparisons_produce_bool_or_null() {
        let schema = Schema::new(vec![Column::new("a", DataType::Int)]);
        let row = vec![Value::Int(2)];
        let expr = Expr::Binary {
            op: BinaryOp::Gt,
            left: Box::new(Expr::Column(0)),
            right: Box::new(Expr::Literal(Value::Int(1))),
        };
        assert_eq!(eval(&expr, &schema, &row).expect("eval"), Value::Bool(true));

        let null_expr = Expr::Binary {
            op: BinaryOp::Eq,
            left: Box::new(Expr::Column(0)),
            right: Box::new(Expr::Literal(Value::Null)),
        };
        assert_eq!(eval(&null_expr, &schema, &row).expect("eval"), Value::Null);
    }

    #[test]
    fn is_null_and_is_unknown_are_boolean() {
        let schema = Schema::new(vec![Column::new("a", DataType::Int)]);
        let row = vec![Value::Null];
        let is_null = Expr::IsNull {
            expr: Box::new(Expr::Column(0)),
            negated: false,
        };
        assert_eq!(
            eval(&is_null, &schema, &row).expect("eval"),
            Value::Bool(true)
        );

        let is_unknown = Expr::IsTruth {
            expr: Box::new(Expr::Column(0)),
            expected: TruthValue::Unknown,
            negated: false,
        };
        assert_eq!(
            eval(&is_unknown, &schema, &row).expect("eval"),
            Value::Bool(true)
        );
    }

    #[test]
    fn arithmetic_propagates_null_and_checks_division() {
        let schema = Schema::new(vec![Column::new("a", DataType::Int)]);
        let row = vec![Value::Int(10)];
        let null_add = Expr::Binary {
            op: BinaryOp::Add,
            left: Box::new(Expr::Literal(Value::Null)),
            right: Box::new(Expr::Column(0)),
        };
        assert_eq!(eval(&null_add, &schema, &row).expect("eval"), Value::Null);

        let div_zero = Expr::Binary {
            op: BinaryOp::Div,
            left: Box::new(Expr::Column(0)),
            right: Box::new(Expr::Literal(Value::Int(0))),
        };
        assert!(matches!(
            eval(&div_zero, &schema, &row),
            Err(ExecError::DivisionByZero)
        ));
    }

    #[test]
    fn in_list_uses_three_valued_logic() {
        let schema = Schema::new(vec![Column::new("a", DataType::Int)]);
        let row = vec![Value::Int(1)];
        let probe = || Box::new(Expr::Column(0));
        let list = |values: Vec<Value>, negated: bool| Expr::InList {
            expr: probe(),
            values: values.into_iter().map(Expr::Literal).collect(),
            negated,
        };
        assert_eq!(
            eval(
                &list(vec![Value::Int(1), Value::Int(3)], false),
                &schema,
                &row
            )
            .expect("in"),
            Value::Bool(true)
        );
        assert_eq!(
            eval(
                &list(vec![Value::Int(2), Value::Int(3)], false),
                &schema,
                &row
            )
            .expect("in"),
            Value::Bool(false)
        );
        assert_eq!(
            eval(
                &list(vec![Value::Int(2), Value::Null], false),
                &schema,
                &row
            )
            .expect("in"),
            Value::Null
        );
        assert_eq!(
            eval(&list(vec![Value::Int(2), Value::Null], true), &schema, &row).expect("not in"),
            Value::Null
        );
        assert_eq!(
            eval(
                &list(vec![Value::Int(1), Value::Int(2)], true),
                &schema,
                &row
            )
            .expect("not in"),
            Value::Bool(false)
        );
    }

    #[test]
    fn between_is_three_valued_conjunction() {
        let schema = Schema::new(vec![Column::new("a", DataType::Int)]);
        let row = vec![Value::Int(20)];
        let between = |low: Value, high: Value, negated: bool| Expr::Between {
            expr: Box::new(Expr::Column(0)),
            low: Box::new(Expr::Literal(low)),
            high: Box::new(Expr::Literal(high)),
            negated,
        };
        assert_eq!(
            eval(
                &between(Value::Int(10), Value::Int(30), false),
                &schema,
                &row
            )
            .expect("between"),
            Value::Bool(true)
        );
        assert_eq!(
            eval(
                &between(Value::Int(21), Value::Int(30), false),
                &schema,
                &row
            )
            .expect("between"),
            Value::Bool(false)
        );
        assert_eq!(
            eval(&between(Value::Null, Value::Int(30), false), &schema, &row).expect("between"),
            Value::Null
        );
        assert_eq!(
            eval(&between(Value::Int(21), Value::Null, false), &schema, &row).expect("between"),
            Value::Bool(false)
        );
        assert_eq!(
            eval(
                &between(Value::Int(21), Value::Int(30), true),
                &schema,
                &row
            )
            .expect("not between"),
            Value::Bool(true)
        );
    }

    #[test]
    fn like_matches_wildcards_and_ascii_case() {
        assert!(like_matches("abc", "a%"));
        assert!(like_matches("abc", "A%"));
        assert!(like_matches("abc", "a_c"));
        assert!(!like_matches("abc", "a_"));
        assert!(like_matches("", "%"));
        assert!(like_matches("abc", "%b%"));
        assert!(!like_matches("abc", "abd"));
        assert!(like_matches("中", "_"));
        assert!(!like_matches("中文", "_"));

        let schema = Schema::new(vec![Column::new("a", DataType::Text)]);
        let row = vec![Value::Null];
        let like = Expr::Like {
            expr: Box::new(Expr::Column(0)),
            pattern: Box::new(Expr::Literal(Value::Text("a%".to_string()))),
            negated: false,
        };
        assert_eq!(eval(&like, &schema, &row).expect("like"), Value::Null);

        let row = vec![Value::Int(1)];
        assert!(matches!(
            eval(&like, &schema, &row),
            Err(ExecError::TypeMismatch {
                expected: "TEXT",
                ..
            })
        ));
    }

    #[test]
    fn case_is_three_valued_and_lazily_evaluated() {
        let schema = Schema::new(vec![
            Column::new("flag", DataType::Bool),
            Column::new("score", DataType::Int),
        ]);
        let true_row = vec![Value::Bool(true), Value::Int(10)];
        let null_row = vec![Value::Null, Value::Int(20)];

        let searched = Expr::Case {
            operand: None,
            branches: vec![
                (
                    Expr::Column(0),
                    Expr::Literal(Value::Text("yes".to_string())),
                ),
                (
                    Expr::IsNull {
                        expr: Box::new(Expr::Column(0)),
                        negated: false,
                    },
                    Expr::Literal(Value::Text("unknown".to_string())),
                ),
            ],
            else_expr: Some(Box::new(Expr::Literal(Value::Text("no".to_string())))),
        };
        assert_eq!(
            eval(&searched, &schema, &true_row).expect("true"),
            Value::Text("yes".to_string())
        );
        assert_eq!(
            eval(&searched, &schema, &null_row).expect("null"),
            Value::Text("unknown".to_string())
        );

        let no_else = Expr::Case {
            operand: None,
            branches: vec![(Expr::Column(0), Expr::Literal(Value::Int(1)))],
            else_expr: None,
        };
        assert_eq!(
            eval(&no_else, &schema, &null_row).expect("no else"),
            Value::Null
        );

        let simple = Expr::Case {
            operand: Some(Box::new(Expr::Column(1))),
            branches: vec![
                (Expr::Literal(Value::Int(10)), Expr::Literal(Value::Int(1))),
                (Expr::Literal(Value::Int(20)), Expr::Literal(Value::Int(2))),
            ],
            else_expr: Some(Box::new(Expr::Literal(Value::Int(0)))),
        };
        assert_eq!(
            eval(&simple, &schema, &true_row).expect("simple ten"),
            Value::Int(1)
        );
        assert_eq!(
            eval(&simple, &schema, &null_row).expect("simple twenty"),
            Value::Int(2)
        );

        let lazy = Expr::Case {
            operand: None,
            branches: vec![(
                Expr::Literal(Value::Bool(true)),
                Expr::Literal(Value::Int(1)),
            )],
            else_expr: Some(Box::new(Expr::Binary {
                op: BinaryOp::Div,
                left: Box::new(Expr::Literal(Value::Int(1))),
                right: Box::new(Expr::Literal(Value::Int(0))),
            })),
        };
        assert_eq!(
            eval(&lazy, &schema, &true_row).expect("lazy"),
            Value::Int(1)
        );
    }

    #[test]
    fn non_boolean_predicate_is_rejected() {
        let schema = Schema::new(vec![Column::new("a", DataType::Int)]);
        let row = vec![Value::Int(1)];
        let expr = Expr::Column(0);
        assert!(matches!(
            truth_of(&eval(&expr, &schema, &row).expect("eval")),
            Err(ExecError::NonBooleanPredicate { .. })
        ));
    }
}

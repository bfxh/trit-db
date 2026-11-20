use tritdb_exec::{BinaryOp, DataType, TruthValue, Value};

/// Aggregate function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggregateKind {
    /// `COUNT(*)` or `COUNT(expr)`.
    Count,
    /// `SUM(expr)`.
    Sum,
    /// `MIN(expr)`.
    Min,
    /// `MAX(expr)`.
    Max,
}

impl AggregateKind {
    /// SQL name.
    pub const fn name(self) -> &'static str {
        match self {
            AggregateKind::Count => "COUNT",
            AggregateKind::Sum => "SUM",
            AggregateKind::Min => "MIN",
            AggregateKind::Max => "MAX",
        }
    }
}

/// Parsed, unbound expression.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    /// Literal.
    Literal(Value),
    /// Column reference, optionally qualified by a table name or alias.
    Column {
        /// Qualifier.
        table: Option<String>,
        /// Column name.
        name: String,
    },
    /// `expr IS [NOT] NULL`.
    IsNull {
        /// Input.
        expr: Box<Expr>,
        /// Negated.
        negated: bool,
    },
    /// `expr IS [NOT] TRUE|FALSE|UNKNOWN`.
    IsTruth {
        /// Input.
        expr: Box<Expr>,
        /// Expected truth value.
        expected: TruthValue,
        /// Negated.
        negated: bool,
    },
    /// `NOT expr`.
    Not(Box<Expr>),
    /// `expr [NOT] IN (value, ...)`.
    InList {
        /// Probe expression.
        expr: Box<Expr>,
        /// Candidate expressions.
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
        /// `ELSE` result.
        else_expr: Option<Box<Expr>>,
    },
    /// Binary operator.
    Binary {
        /// Operator.
        op: BinaryOp,
        /// Left.
        left: Box<Expr>,
        /// Right.
        right: Box<Expr>,
    },
    /// Aggregate call. `None` means `COUNT(*)`.
    Aggregate {
        /// Function.
        kind: AggregateKind,
        /// Argument.
        input: Option<Box<Expr>>,
    },
}

/// A `CREATE TABLE` column definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnDef {
    /// Column name.
    pub name: String,
    /// Type.
    pub data_type: DataType,
    /// Reject `NULL`.
    pub not_null: bool,
    /// Reject duplicate non-`NULL` values.
    pub unique: bool,
}

/// Select-list item.
#[derive(Clone, Debug, PartialEq)]
pub enum SelectItem {
    /// `*`.
    Wildcard,
    /// Expression with optional alias.
    Expr {
        /// Expression.
        expr: Expr,
        /// Alias.
        alias: Option<String>,
    },
}

/// `ORDER BY` item.
#[derive(Clone, Debug, PartialEq)]
pub struct OrderBy {
    /// Sort expression.
    pub expr: Expr,
    /// Ascending when true.
    pub ascending: bool,
}

/// Join flavor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinKind {
    /// Inner join: only matching row pairs survive.
    Inner,
    /// Cross join: every left row pairs with every right row.
    Cross,
    /// Left outer join: unmatched left rows survive with `NULL` right columns.
    Left,
    /// Right outer join: unmatched right rows survive with `NULL` left columns.
    Right,
    /// Full outer join: unmatched rows from either side survive with `NULL`
    /// columns from the other side.
    Full,
}

/// Join condition.
#[derive(Clone, Debug, PartialEq)]
pub enum JoinCondition {
    /// No condition: `CROSS JOIN` and comma-separated sources.
    None,
    /// Explicit `ON` predicate.
    On(Expr),
    /// `USING (column, ...)`.
    Using(Vec<String>),
    /// `NATURAL JOIN`.
    Natural,
}

/// A join clause.
#[derive(Clone, Debug, PartialEq)]
pub struct Join {
    /// Join flavor.
    pub kind: JoinKind,
    /// Joined table.
    pub table: String,
    /// Optional alias.
    pub alias: Option<String>,
    /// Join condition.
    pub condition: JoinCondition,
}

/// Parsed `SELECT`.
#[derive(Clone, Debug, PartialEq)]
pub struct Select {
    /// Select list.
    pub items: Vec<SelectItem>,
    /// Source table.
    pub table: String,
    /// Source table alias.
    pub table_alias: Option<String>,
    /// Join clauses, applied left to right.
    pub joins: Vec<Join>,
    /// Optional filter.
    pub filter: Option<Expr>,
    /// Grouping columns.
    pub group_by: Vec<Expr>,
    /// Optional group filter.
    pub having: Option<Expr>,
    /// Sort keys.
    pub order_by: Vec<OrderBy>,
    /// Optional limit.
    pub limit: Option<usize>,
    /// `UNDER REPAIR ON (col, ...)`: keep rows that are certain in every
    /// minimal repair of the functional dependency `cols -> other columns`.
    pub under_repair: Option<Vec<String>>,
}

/// One `SET` assignment in an `UPDATE`.
#[derive(Clone, Debug, PartialEq)]
pub struct Assignment {
    /// Target column.
    pub column: String,
    /// Replacement expression.
    pub value: Expr,
}

/// Parsed statement.
#[derive(Clone, Debug, PartialEq)]
pub enum Statement {
    /// `CREATE TABLE`.
    CreateTable {
        /// Table name.
        name: String,
        /// Skip when the table exists.
        if_not_exists: bool,
        /// Columns.
        columns: Vec<ColumnDef>,
        /// Table-level `UNIQUE (a, b, ...)` groups.
        unique: Vec<Vec<String>>,
    },
    /// `DROP TABLE`.
    DropTable {
        /// Table name.
        name: String,
        /// Skip when the table is absent.
        if_exists: bool,
    },
    /// `ALTER TABLE ... ADD [COLUMN] ...`.
    AlterTableAddColumn {
        /// Table name.
        name: String,
        /// Skip when the column already exists.
        if_not_exists: bool,
        /// Appended column definition.
        column: ColumnDef,
    },
    /// `INSERT INTO ... VALUES`.
    Insert {
        /// Table name.
        table: String,
        /// Optional column list.
        columns: Option<Vec<String>>,
        /// Rows of expressions.
        rows: Vec<Vec<Expr>>,
    },
    /// `SELECT count_models('<expr>'[, vars])`: exact model count over a
    /// boolean expression. This is rule-level, not `COUNT(*)`.
    CountModels {
        /// Boolean expression over `a..z` / `vN` / `N`.
        expr: String,
        /// Declared variable count; `0` infers the maximum index + 1.
        vars: usize,
    },
    /// `SELECT`.
    Select(Select),
    /// `EXPLAIN SELECT`.
    Explain(Select),
    /// `EXPLAIN SEXPR SELECT`: read-only parenthesized plan output.
    ExplainSexpr(Select),
    /// `EXPLAIN ANALYZE SELECT`.
    ExplainAnalyze(Select),
    /// `EXPLAIN ANALYZE SEXPR SELECT`: parenthesized plan plus execution totals.
    ExplainAnalyzeSexpr(Select),
    /// `ANALYZE [table]`: recompute exact table statistics.
    Analyze {
        /// Optional table name; `None` analyzes every table.
        table: Option<String>,
    },
    /// `DELETE FROM`.
    Delete {
        /// Table name.
        table: String,
        /// Optional filter.
        filter: Option<Expr>,
    },
    /// `UPDATE ... SET ...`.
    Update {
        /// Table name.
        table: String,
        /// Assignments in order.
        assignments: Vec<Assignment>,
        /// Optional filter.
        filter: Option<Expr>,
    },
}

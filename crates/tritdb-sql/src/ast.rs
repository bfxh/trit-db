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
    /// Left outer join: unmatched left rows survive with `NULL` right columns.
    Left,
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
    /// Join predicate.
    pub on: Expr,
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
    /// Inner joins.
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
    },
    /// `DROP TABLE`.
    DropTable {
        /// Table name.
        name: String,
        /// Skip when the table is absent.
        if_exists: bool,
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
    /// `SELECT`.
    Select(Select),
    /// `DELETE FROM`.
    Delete {
        /// Table name.
        table: String,
        /// Optional filter.
        filter: Option<Expr>,
    },
}

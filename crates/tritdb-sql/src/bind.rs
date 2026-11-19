use std::collections::{BTreeMap, BTreeSet};

use tritdb_exec::{eval, BinaryOp, Expr as ExecExpr, Schema, Table, TruthValue, Value};

use crate::ast::{AggregateKind, Expr, Select, SelectItem};
use crate::error::SqlError;

/// A bound expression that may contain aggregate calls.
#[derive(Clone, Debug)]
pub(crate) enum GroupExpr {
    /// Scalar expression evaluated against a source row.
    Scalar(ExecExpr),
    /// Aggregate call.
    Aggregate {
        /// Function.
        kind: AggregateKind,
        /// Argument; `None` for `COUNT(*)`.
        input: Option<ExecExpr>,
    },
    /// `expr IS [NOT] NULL`.
    IsNull {
        /// Input.
        expr: Box<GroupExpr>,
        /// Negated.
        negated: bool,
    },
    /// `expr IS [NOT] TRUE|FALSE|UNKNOWN`.
    IsTruth {
        /// Input.
        expr: Box<GroupExpr>,
        /// Expected truth value.
        expected: TruthValue,
        /// Negated.
        negated: bool,
    },
    /// `NOT expr`.
    Not(Box<GroupExpr>),
    /// Binary operator.
    Binary {
        /// Operator.
        op: BinaryOp,
        /// Left.
        left: Box<GroupExpr>,
        /// Right.
        right: Box<GroupExpr>,
    },
}

impl GroupExpr {
    pub(crate) fn has_aggregate(&self) -> bool {
        match self {
            GroupExpr::Scalar(_) => false,
            GroupExpr::Aggregate { .. } => true,
            GroupExpr::IsNull { expr, .. } | GroupExpr::IsTruth { expr, .. } => {
                expr.has_aggregate()
            }
            GroupExpr::Not(expr) => expr.has_aggregate(),
            GroupExpr::Binary { left, right, .. } => left.has_aggregate() || right.has_aggregate(),
        }
    }
}

/// A bound select item.
#[derive(Clone, Debug)]
pub(crate) struct BoundItem {
    /// Bound expression.
    pub(crate) expr: GroupExpr,
    /// Output name.
    pub(crate) name: String,
}

impl BoundItem {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }
}

/// A bound sort key.
#[derive(Clone, Debug)]
pub(crate) struct BoundOrder {
    /// Expression evaluated against a row or a group.
    pub(crate) expr: GroupExpr,
    /// Ascending.
    pub(crate) ascending: bool,
}

/// A fully bound SELECT.
#[derive(Clone, Debug)]
pub(crate) struct BoundSelect {
    /// Filter.
    pub(crate) filter: Option<ExecExpr>,
    /// Projection list.
    pub(crate) items: Vec<BoundItem>,
    /// Grouping columns, empty for a global aggregate.
    pub(crate) group_by: Vec<ExecExpr>,
    /// Group filter.
    pub(crate) having: Option<GroupExpr>,
    /// Sort keys.
    pub(crate) order_by: Vec<BoundOrder>,
    /// Limit.
    pub(crate) limit: Option<usize>,
}

impl BoundSelect {
    pub(crate) fn has_aggregate(&self) -> bool {
        self.items.iter().any(|item| item.expr.has_aggregate())
            || self.having.as_ref().is_some_and(GroupExpr::has_aggregate)
            || self.order_by.iter().any(|order| order.expr.has_aggregate())
    }

    pub(crate) fn is_grouped(&self) -> bool {
        !self.group_by.is_empty() || self.has_aggregate()
    }
}

/// Resolves a column reference against a schema.
fn resolve_column(schema: &Schema, table: Option<&str>, name: &str) -> Result<usize, SqlError> {
    if let Some(table) = table {
        let qualified = format!("{table}.{name}");
        return schema
            .index_of(&qualified)
            .ok_or(SqlError::UnknownColumn(qualified));
    }
    if let Some(index) = schema.index_of(name) {
        return Ok(index);
    }
    let mut matches = schema
        .columns
        .iter()
        .enumerate()
        .filter_map(|(index, column)| {
            column
                .name
                .rsplit_once('.')
                .filter(|(_, suffix)| *suffix == name)
                .map(|_| index)
        });
    let Some(index) = matches.next() else {
        return Err(SqlError::UnknownColumn(name.to_string()));
    };
    if matches.next().is_some() {
        return Err(SqlError::AmbiguousColumn(name.to_string()));
    }
    Ok(index)
}

fn displayed_column(name: &str) -> &str {
    name.rsplit_once('.').map_or(name, |(_, column)| column)
}

fn column_name(table: Option<&str>, name: &str) -> String {
    table.map_or_else(|| name.to_string(), |table| format!("{table}.{name}"))
}

/// Binds an expression to a schema.
pub(crate) fn bind_expr(expr: &Expr, schema: &Schema) -> Result<ExecExpr, SqlError> {
    match expr {
        Expr::Literal(value) => Ok(ExecExpr::Literal(value.clone())),
        Expr::Column { table, name } => {
            let index = resolve_column(schema, table.as_deref(), name)?;
            Ok(ExecExpr::Column(index))
        }
        Expr::IsNull { expr, negated } => Ok(ExecExpr::IsNull {
            expr: Box::new(bind_expr(expr, schema)?),
            negated: *negated,
        }),
        Expr::IsTruth {
            expr,
            expected,
            negated,
        } => Ok(ExecExpr::IsTruth {
            expr: Box::new(bind_expr(expr, schema)?),
            expected: *expected,
            negated: *negated,
        }),
        Expr::Not(expr) => Ok(ExecExpr::Not(Box::new(bind_expr(expr, schema)?))),
        Expr::Binary { op, left, right } => Ok(ExecExpr::Binary {
            op: *op,
            left: Box::new(bind_expr(left, schema)?),
            right: Box::new(bind_expr(right, schema)?),
        }),
        Expr::Aggregate { .. } => Err(SqlError::Unsupported(
            "aggregate is only allowed in SELECT, HAVING, or ORDER BY".to_string(),
        )),
    }
}

/// Evaluates a constant expression with no row context.
pub(crate) fn eval_constant(expr: &Expr) -> Result<Value, SqlError> {
    let bound = bind_expr(expr, &Schema::default())?;
    Ok(eval(&bound, &Schema::default(), &[])?)
}

/// Default output label for an aggregate.
fn aggregate_label(kind: AggregateKind, input: Option<&Expr>) -> String {
    match input {
        Some(input) => format!("{}({})", kind.name(), expr_label(input)),
        None => "COUNT(*)".to_string(),
    }
}

fn expr_label(expr: &Expr) -> String {
    match expr {
        Expr::Column { name, .. } => name.clone(),
        Expr::Literal(value) => value.display(),
        Expr::Aggregate { kind, input } => aggregate_label(*kind, input.as_deref()),
        _ => "expr".to_string(),
    }
}

fn expr_has_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Literal(_) | Expr::Column { .. } => false,
        Expr::IsNull { expr, .. } | Expr::IsTruth { expr, .. } => expr_has_aggregate(expr),
        Expr::Not(expr) => expr_has_aggregate(expr),
        Expr::Binary { left, right, .. } => expr_has_aggregate(left) || expr_has_aggregate(right),
        Expr::Aggregate { .. } => true,
    }
}

fn select_has_aggregate(select: &Select) -> bool {
    select.items.iter().any(|item| match item {
        SelectItem::Wildcard => false,
        SelectItem::Expr { expr, .. } => expr_has_aggregate(expr),
    }) || select.having.as_ref().is_some_and(expr_has_aggregate)
        || select
            .order_by
            .iter()
            .any(|order| expr_has_aggregate(&order.expr))
}

fn bind_group_expr(
    expr: &Expr,
    schema: &Schema,
    grouped: bool,
    aggregate_query: bool,
    group_columns: &BTreeSet<usize>,
) -> Result<GroupExpr, SqlError> {
    match expr {
        Expr::Literal(value) => Ok(GroupExpr::Scalar(ExecExpr::Literal(value.clone()))),
        Expr::Column { table, name } => {
            let index = resolve_column(schema, table.as_deref(), name)?;
            if aggregate_query && !grouped {
                return Err(SqlError::ColumnOutsideAggregate(column_name(
                    table.as_deref(),
                    name,
                )));
            }
            if grouped && !group_columns.contains(&index) {
                return Err(SqlError::ColumnOutsideGroup(column_name(
                    table.as_deref(),
                    name,
                )));
            }
            Ok(GroupExpr::Scalar(ExecExpr::Column(index)))
        }
        Expr::IsNull { expr, negated } => Ok(GroupExpr::IsNull {
            expr: Box::new(bind_group_expr(
                expr,
                schema,
                grouped,
                aggregate_query,
                group_columns,
            )?),
            negated: *negated,
        }),
        Expr::IsTruth {
            expr,
            expected,
            negated,
        } => Ok(GroupExpr::IsTruth {
            expr: Box::new(bind_group_expr(
                expr,
                schema,
                grouped,
                aggregate_query,
                group_columns,
            )?),
            expected: *expected,
            negated: *negated,
        }),
        Expr::Not(expr) => Ok(GroupExpr::Not(Box::new(bind_group_expr(
            expr,
            schema,
            grouped,
            aggregate_query,
            group_columns,
        )?))),
        Expr::Binary { op, left, right } => Ok(GroupExpr::Binary {
            op: *op,
            left: Box::new(bind_group_expr(
                left,
                schema,
                grouped,
                aggregate_query,
                group_columns,
            )?),
            right: Box::new(bind_group_expr(
                right,
                schema,
                grouped,
                aggregate_query,
                group_columns,
            )?),
        }),
        Expr::Aggregate { kind, input } => {
            let input = match input {
                Some(input) => {
                    if expr_has_aggregate(input) {
                        return Err(SqlError::NestedAggregate);
                    }
                    Some(bind_expr(input, schema)?)
                }
                None => None,
            };
            Ok(GroupExpr::Aggregate { kind: *kind, input })
        }
    }
}

/// Binds `SELECT` against a table.
pub(crate) fn bind_select(select: &Select, table: &Table) -> Result<BoundSelect, SqlError> {
    let schema = &table.schema;

    let mut group_by = Vec::new();
    let mut group_columns = BTreeSet::new();
    for expr in &select.group_by {
        let Expr::Column { table, name } = expr else {
            return Err(SqlError::Unsupported(
                "GROUP BY only supports column references".to_string(),
            ));
        };
        let index = resolve_column(schema, table.as_deref(), name)?;
        if group_columns.insert(index) {
            group_by.push(ExecExpr::Column(index));
        }
    }
    let grouped = !group_by.is_empty();
    let aggregate_query = grouped || select_has_aggregate(select);

    let mut items = Vec::new();
    for (position, item) in select.items.iter().enumerate() {
        match item {
            SelectItem::Wildcard => {
                if aggregate_query {
                    return Err(SqlError::Unsupported(
                        "SELECT * with GROUP BY or aggregate is not supported".to_string(),
                    ));
                }
                for (index, column) in schema.columns.iter().enumerate() {
                    items.push(BoundItem {
                        expr: GroupExpr::Scalar(ExecExpr::Column(index)),
                        name: displayed_column(&column.name).to_string(),
                    });
                }
            }
            SelectItem::Expr { expr, alias } => {
                let name = alias
                    .clone()
                    .or_else(|| match expr {
                        Expr::Column { name, .. } => Some(name.clone()),
                        Expr::Aggregate { kind, input } => {
                            Some(aggregate_label(*kind, input.as_deref()))
                        }
                        _ => None,
                    })
                    .unwrap_or_else(|| format!("col{}", position + 1));
                items.push(BoundItem {
                    expr: bind_group_expr(expr, schema, grouped, aggregate_query, &group_columns)?,
                    name,
                });
            }
        }
    }

    if items.is_empty() {
        return Err(SqlError::Unsupported("empty SELECT list".to_string()));
    }

    let filter = select
        .filter
        .as_ref()
        .map(|expr| bind_expr(expr, schema))
        .transpose()?;

    let having = match &select.having {
        Some(having) => {
            if !aggregate_query {
                return Err(SqlError::Unsupported(
                    "HAVING requires GROUP BY or an aggregate".to_string(),
                ));
            }
            Some(bind_group_expr(
                having,
                schema,
                grouped,
                aggregate_query,
                &group_columns,
            )?)
        }
        None => None,
    };

    let aliases: BTreeMap<&str, &GroupExpr> =
        items.iter().map(|item| (item.name(), &item.expr)).collect();

    let mut order_by = Vec::new();
    for order in &select.order_by {
        let expr = if let Expr::Column { table, name } = &order.expr {
            if table.is_some() {
                bind_group_expr(
                    &order.expr,
                    schema,
                    grouped,
                    aggregate_query,
                    &group_columns,
                )?
            } else {
                match resolve_column(schema, None, name) {
                    Ok(_) => bind_group_expr(
                        &order.expr,
                        schema,
                        grouped,
                        aggregate_query,
                        &group_columns,
                    )?,
                    Err(SqlError::UnknownColumn(_)) => {
                        if let Some(alias) = aliases.get(name.as_str()) {
                            (*alias).clone()
                        } else {
                            return Err(SqlError::UnknownColumn(name.clone()));
                        }
                    }
                    Err(error) => return Err(error),
                }
            }
        } else {
            bind_group_expr(
                &order.expr,
                schema,
                grouped,
                aggregate_query,
                &group_columns,
            )?
        };
        order_by.push(BoundOrder {
            expr,
            ascending: order.ascending,
        });
    }

    Ok(BoundSelect {
        filter,
        items,
        group_by,
        having,
        order_by,
        limit: select.limit,
    })
}

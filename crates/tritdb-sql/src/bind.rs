use std::collections::BTreeMap;

use tritdb_exec::{eval, Expr as ExecExpr, Schema, Table, Value};

use crate::ast::{AggregateKind, Expr, Select, SelectItem};
use crate::error::SqlError;

/// A bound select item.
#[derive(Clone, Debug)]
pub(crate) enum BoundItem {
    /// Scalar projection.
    Expr {
        /// Bound expression.
        expr: ExecExpr,
        /// Output name.
        name: String,
    },
    /// Aggregate projection.
    Aggregate {
        /// Function.
        kind: AggregateKind,
        /// Argument; `None` for `COUNT(*)`.
        input: Option<ExecExpr>,
        /// Output name.
        name: String,
    },
}

impl BoundItem {
    pub(crate) fn name(&self) -> &str {
        match self {
            BoundItem::Expr { name, .. } | BoundItem::Aggregate { name, .. } => name,
        }
    }
}

/// A bound sort key.
#[derive(Clone, Debug)]
pub(crate) struct BoundOrder {
    /// Source-row expression.
    pub(crate) expr: ExecExpr,
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
    /// Sort keys.
    pub(crate) order_by: Vec<BoundOrder>,
    /// Limit.
    pub(crate) limit: Option<usize>,
}

impl BoundSelect {
    pub(crate) fn has_aggregate(&self) -> bool {
        self.items
            .iter()
            .any(|item| matches!(item, BoundItem::Aggregate { .. }))
    }
}

/// Binds an expression to a schema.
pub(crate) fn bind_expr(expr: &Expr, schema: &Schema) -> Result<ExecExpr, SqlError> {
    match expr {
        Expr::Literal(value) => Ok(ExecExpr::Literal(value.clone())),
        Expr::Column(name) => {
            let index = schema
                .index_of(name)
                .ok_or_else(|| SqlError::UnknownColumn(name.clone()))?;
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
            "aggregate is only allowed as a top-level SELECT item".to_string(),
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
        Expr::Column(name) => name.clone(),
        Expr::Literal(value) => value.display(),
        Expr::Aggregate { kind, input } => aggregate_label(*kind, input.as_deref()),
        _ => "expr".to_string(),
    }
}

/// Binds `SELECT` against a table.
pub(crate) fn bind_select(select: &Select, table: &Table) -> Result<BoundSelect, SqlError> {
    let schema = &table.schema;
    let mut items = Vec::new();
    for (position, item) in select.items.iter().enumerate() {
        match item {
            SelectItem::Wildcard => {
                for (index, column) in schema.columns.iter().enumerate() {
                    items.push(BoundItem::Expr {
                        expr: ExecExpr::Column(index),
                        name: column.name.clone(),
                    });
                }
            }
            SelectItem::Expr { expr, alias } => {
                let name = alias
                    .clone()
                    .or_else(|| match expr {
                        Expr::Column(name) => Some(name.clone()),
                        Expr::Aggregate { kind, input } => {
                            Some(aggregate_label(*kind, input.as_deref()))
                        }
                        _ => None,
                    })
                    .unwrap_or_else(|| format!("col{}", position + 1));
                match expr {
                    Expr::Aggregate { kind, input } => {
                        let input = match input {
                            Some(input) => {
                                if matches!(**input, Expr::Aggregate { .. }) {
                                    return Err(SqlError::NestedAggregate);
                                }
                                Some(bind_expr(input, schema)?)
                            }
                            None => None,
                        };
                        items.push(BoundItem::Aggregate {
                            kind: *kind,
                            input,
                            name,
                        });
                    }
                    other => {
                        items.push(BoundItem::Expr {
                            expr: bind_expr(other, schema)?,
                            name,
                        });
                    }
                }
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

    let mut aliases: BTreeMap<&str, &ExecExpr> = BTreeMap::new();
    for item in &items {
        if let BoundItem::Expr { expr, name } = item {
            aliases.insert(name.as_str(), expr);
        }
    }

    let mut order_by = Vec::new();
    for order in &select.order_by {
        let expr = if let Expr::Column(name) = &order.expr {
            if schema.index_of(name).is_none() {
                if let Some(alias) = aliases.get(name.as_str()) {
                    (*alias).clone()
                } else {
                    return Err(SqlError::UnknownColumn(name.clone()));
                }
            } else {
                bind_expr(&order.expr, schema)?
            }
        } else {
            bind_expr(&order.expr, schema)?
        };
        order_by.push(BoundOrder {
            expr,
            ascending: order.ascending,
        });
    }

    let bound = BoundSelect {
        filter,
        items,
        order_by,
        limit: select.limit,
    };
    if bound.has_aggregate() && !bound.order_by.is_empty() {
        return Err(SqlError::Unsupported(
            "ORDER BY with aggregate is not supported".to_string(),
        ));
    }
    Ok(bound)
}

/// Checks that an expression has no column references.
pub(crate) fn has_column(expr: &ExecExpr) -> bool {
    match expr {
        ExecExpr::Column(_) => true,
        ExecExpr::Literal(_) => false,
        ExecExpr::IsNull { expr, .. } | ExecExpr::IsTruth { expr, .. } => has_column(expr),
        ExecExpr::Not(expr) => has_column(expr),
        ExecExpr::Binary { left, right, .. } => has_column(left) || has_column(right),
    }
}

//! Structured representation of the text emitted by `EXPLAIN`.
//!
//! The tree is deliberately small: it captures the operators and the
//! observability fields the executor already exposes, without pretending to be
//! a full logical or physical plan. `to_text` preserves the historical
//! `EXPLAIN` text byte-for-byte; `to_sexpr` is a read-only, data-oriented view
//! intended for debugging and future rule matching.

use crate::ast::JoinKind;
use crate::error::SqlError;
use tritdb_exec::{BinaryOp, Expr as ExecExpr, Schema, TruthValue, Value};

/// A plan node with zero, one, or two children.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PlanNode {
    pub(crate) kind: PlanKind,
    pub(crate) children: Vec<PlanNode>,
}

/// One operator and its observable fields.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PlanKind {
    Scan {
        label: String,
        filtered: bool,
        actual_rows: Option<u64>,
        est_rows: Option<u64>,
        columns: Option<Vec<String>>,
    },
    Join(JoinPlan),
    Filter {
        pred: PlanExpr,
        metrics: Metrics,
    },
    Aggregate(Metrics),
    Having(Metrics),
    Project {
        columns: Vec<String>,
        metrics: Metrics,
    },
    Sort {
        keys: usize,
        metrics: Metrics,
    },
    Limit {
        limit: usize,
        actual_rows: Option<u64>,
        est_rows: Option<u64>,
    },
}

/// A bound predicate rendered as data. Column indexes from the executor
/// expression are resolved back to schema names so the plan remains readable
/// without the binder context.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PlanExpr {
    Literal(Value),
    Column(String),
    IsNull {
        expr: Box<PlanExpr>,
        negated: bool,
    },
    IsTruth {
        expr: Box<PlanExpr>,
        expected: TruthValue,
        negated: bool,
    },
    Not(Box<PlanExpr>),
    InList {
        expr: Box<PlanExpr>,
        values: Vec<PlanExpr>,
        negated: bool,
    },
    Between {
        expr: Box<PlanExpr>,
        low: Box<PlanExpr>,
        high: Box<PlanExpr>,
        negated: bool,
    },
    Like {
        expr: Box<PlanExpr>,
        pattern: Box<PlanExpr>,
        negated: bool,
    },
    Case {
        operand: Option<Box<PlanExpr>>,
        branches: Vec<(PlanExpr, PlanExpr)>,
        else_expr: Option<Box<PlanExpr>>,
    },
    Binary {
        op: BinaryOp,
        left: Box<PlanExpr>,
        right: Box<PlanExpr>,
    },
}

/// Join-specific fields. Keys and residual flags are data; the historical
/// `keys=...` / `residual=yes` / `on=expr` fragments are rendered on demand.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct JoinPlan {
    pub(crate) uses_hash: bool,
    pub(crate) kind: JoinKind,
    pub(crate) keys: Option<JoinKeys>,
    /// `true` when an `ON` hash join has a predicate beyond its equi-keys.
    pub(crate) residual: bool,
    /// `true` when a nested-loop `ON` join shows the whole predicate as
    /// `on=expr`.
    pub(crate) on_expr: bool,
    /// Complete bound `ON` / `USING` / `NATURAL` predicate, when one exists.
    pub(crate) pred: Option<PlanExpr>,
    pub(crate) metrics: Metrics,
}

/// How a join's keys are displayed. `USING` / `NATURAL` keys are symbolic
/// names shared by both sides; explicit `ON` equalities retain both qualified
/// column names.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum JoinKeys {
    Names(Vec<String>),
    Pairs(Vec<JoinKey>),
}

/// One explicit equality key, e.g. `a.id = b.id`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct JoinKey {
    pub(crate) left: String,
    pub(crate) right: String,
}

/// Observability fields shared by unary operators. `actual` and `est` are rows
/// for `Filter`/`Project`/`Sort`, and groups for `Aggregate`/`Having`.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Metrics {
    pub(crate) actual: Option<u64>,
    pub(crate) actual_ms: Option<f64>,
    pub(crate) est: Option<u64>,
}

impl PlanNode {
    pub(crate) fn scan(
        label: String,
        filtered: bool,
        actual_rows: Option<u64>,
        est_rows: Option<u64>,
        columns: Option<Vec<String>>,
    ) -> Self {
        Self {
            kind: PlanKind::Scan {
                label,
                filtered,
                actual_rows,
                est_rows,
                columns,
            },
            children: Vec::new(),
        }
    }

    pub(crate) fn join(join: JoinPlan, left: Self, right: Self) -> Self {
        Self {
            kind: PlanKind::Join(join),
            children: vec![left, right],
        }
    }

    pub(crate) fn unary(kind: PlanKind, child: Self) -> Self {
        Self {
            kind,
            children: vec![child],
        }
    }

    /// Render the historical `EXPLAIN` text.
    pub(crate) fn to_text(&self) -> String {
        let mut out = String::new();
        self.write_text(&mut out, 0);
        out
    }

    /// Render a parenthesized, data-oriented plan tree. This is not a Lisp
    /// evaluator: it only serializes an already-built plan.
    pub(crate) fn to_sexpr(&self) -> String {
        let mut out = String::new();
        self.write_sexpr(&mut out, 0);
        out
    }

    /// Render a plan tree with a leading execution envelope. The envelope is
    /// only used by `EXPLAIN ANALYZE SEXPR`; it carries the same totals as the
    /// text format's final `Execution:` line.
    pub(crate) fn to_sexpr_with_execution(&self, rows: usize, total_ms: f64) -> String {
        let body = self.to_sexpr();
        let mut out = format!("(execution :rows {rows} :time-ms {total_ms:.3}\n");
        for line in body.lines() {
            out.push_str("  ");
            out.push_str(line);
            out.push('\n');
        }
        out.pop();
        out.push(')');
        out
    }

    fn write_text(&self, out: &mut String, indent: usize) {
        out.push_str(&"  ".repeat(indent));
        out.push_str(&self.label());
        for child in &self.children {
            out.push('\n');
            child.write_text(out, indent + 1);
        }
    }

    fn label(&self) -> String {
        match &self.kind {
            PlanKind::Scan {
                label,
                filtered,
                actual_rows,
                est_rows,
                columns,
            } => {
                let mut text = format!("Scan {label}");
                if *filtered {
                    text.push_str(" filtered");
                }
                push_metrics(
                    &mut text,
                    &Metrics {
                        actual: *actual_rows,
                        actual_ms: None,
                        est: *est_rows,
                    },
                    "rows",
                );
                if let Some(columns) = columns {
                    if columns.is_empty() {
                        text.push_str(" cols=()");
                    } else {
                        text.push_str(&format!(" cols={}", columns.join(",")));
                    }
                }
                text
            }
            PlanKind::Join(join) => {
                let mut text = format!(
                    "{} ({})",
                    if join.uses_hash {
                        "HashJoin"
                    } else {
                        "NestedLoop"
                    },
                    join_kind_name(join.kind)
                );
                let fragments = join.detail_fragments();
                if !fragments.is_empty() {
                    text.push(' ');
                    text.push_str(&fragments.join(" "));
                }
                push_metrics(&mut text, &join.metrics, "rows");
                text
            }
            PlanKind::Filter { metrics, .. } => {
                let mut text = "Filter".to_string();
                push_metrics(&mut text, metrics, "rows");
                text
            }
            PlanKind::Aggregate(metrics) => {
                let mut text = "Aggregate".to_string();
                push_metrics(&mut text, metrics, "groups");
                text
            }
            PlanKind::Having(metrics) => {
                let mut text = "Having".to_string();
                push_metrics(&mut text, metrics, "groups");
                text
            }
            PlanKind::Project { columns, metrics } => {
                let mut text = format!("Project [{}]", columns.join(", "));
                push_metrics(&mut text, metrics, "rows");
                text
            }
            PlanKind::Sort { keys, metrics } => {
                let mut text = format!("Sort ({keys} key(s))");
                push_metrics(&mut text, metrics, "rows");
                text
            }
            PlanKind::Limit {
                limit,
                actual_rows,
                est_rows,
            } => {
                let mut text = format!("Limit {limit}");
                push_metrics(
                    &mut text,
                    &Metrics {
                        actual: *actual_rows,
                        actual_ms: None,
                        est: *est_rows,
                    },
                    "rows",
                );
                text
            }
        }
    }

    fn write_sexpr(&self, out: &mut String, indent: usize) {
        out.push_str(&"  ".repeat(indent));
        match &self.kind {
            PlanKind::Scan {
                label,
                filtered,
                actual_rows,
                est_rows,
                columns,
            } => {
                out.push_str("(scan ");
                push_string(out, label);
                if *filtered {
                    out.push_str(" :filtered #t");
                }
                push_sexpr_metrics(
                    out,
                    &Metrics {
                        actual: *actual_rows,
                        actual_ms: None,
                        est: *est_rows,
                    },
                    "rows",
                );
                if let Some(columns) = columns {
                    out.push_str(" :cols ");
                    push_string_list(out, columns);
                }
                out.push(')');
            }
            PlanKind::Join(join) => {
                out.push_str("(join ");
                out.push_str(if join.uses_hash {
                    "hash"
                } else {
                    "nested-loop"
                });
                out.push(' ');
                out.push_str(join_kind_name(join.kind));
                if let Some(keys) = &join.keys {
                    out.push_str(" :keys ");
                    push_join_keys(out, keys);
                }
                if join.residual {
                    out.push_str(" :residual #t");
                }
                if join.on_expr {
                    out.push_str(" :on-expr #t");
                }
                if let Some(pred) = &join.pred {
                    out.push_str(" :pred ");
                    pred.write_sexpr(out);
                }
                push_sexpr_metrics(out, &join.metrics, "rows");
                self.write_sexpr_children(out, indent);
                out.push(')');
            }
            PlanKind::Filter { pred, metrics } => {
                out.push_str("(filter :pred ");
                pred.write_sexpr(out);
                push_sexpr_metrics(out, metrics, "rows");
                self.write_sexpr_children(out, indent);
                out.push(')');
            }
            PlanKind::Aggregate(metrics) => {
                out.push_str("(aggregate");
                push_sexpr_metrics(out, metrics, "groups");
                self.write_sexpr_children(out, indent);
                out.push(')');
            }
            PlanKind::Having(metrics) => {
                out.push_str("(having");
                push_sexpr_metrics(out, metrics, "groups");
                self.write_sexpr_children(out, indent);
                out.push(')');
            }
            PlanKind::Project { columns, metrics } => {
                out.push_str("(project ");
                push_string_list(out, columns);
                push_sexpr_metrics(out, metrics, "rows");
                self.write_sexpr_children(out, indent);
                out.push(')');
            }
            PlanKind::Sort { keys, metrics } => {
                out.push_str(&format!("(sort :keys {keys}"));
                push_sexpr_metrics(out, metrics, "rows");
                self.write_sexpr_children(out, indent);
                out.push(')');
            }
            PlanKind::Limit {
                limit,
                actual_rows,
                est_rows,
            } => {
                out.push_str(&format!("(limit :limit {limit}"));
                push_sexpr_metrics(
                    out,
                    &Metrics {
                        actual: *actual_rows,
                        actual_ms: None,
                        est: *est_rows,
                    },
                    "rows",
                );
                self.write_sexpr_children(out, indent);
                out.push(')');
            }
        }
    }

    fn write_sexpr_children(&self, out: &mut String, indent: usize) {
        for child in &self.children {
            out.push('\n');
            child.write_sexpr(out, indent + 1);
        }
    }
}

impl PlanExpr {
    pub(crate) fn from_exec(expr: &ExecExpr, schema: &Schema) -> Result<Self, SqlError> {
        match expr {
            ExecExpr::Literal(value) => Ok(Self::Literal(value.clone())),
            ExecExpr::Column(index) => schema
                .columns
                .get(*index)
                .map(|column| Self::Column(column.name.clone()))
                .ok_or_else(|| SqlError::UnknownColumn(format!("column index {index}"))),
            ExecExpr::IsNull { expr, negated } => Ok(Self::IsNull {
                expr: Box::new(Self::from_exec(expr, schema)?),
                negated: *negated,
            }),
            ExecExpr::IsTruth {
                expr,
                expected,
                negated,
            } => Ok(Self::IsTruth {
                expr: Box::new(Self::from_exec(expr, schema)?),
                expected: *expected,
                negated: *negated,
            }),
            ExecExpr::Not(expr) => Ok(Self::Not(Box::new(Self::from_exec(expr, schema)?))),
            ExecExpr::InList {
                expr,
                values,
                negated,
            } => Ok(Self::InList {
                expr: Box::new(Self::from_exec(expr, schema)?),
                values: values
                    .iter()
                    .map(|value| Self::from_exec(value, schema))
                    .collect::<Result<Vec<_>, _>>()?,
                negated: *negated,
            }),
            ExecExpr::Between {
                expr,
                low,
                high,
                negated,
            } => Ok(Self::Between {
                expr: Box::new(Self::from_exec(expr, schema)?),
                low: Box::new(Self::from_exec(low, schema)?),
                high: Box::new(Self::from_exec(high, schema)?),
                negated: *negated,
            }),
            ExecExpr::Like {
                expr,
                pattern,
                negated,
            } => Ok(Self::Like {
                expr: Box::new(Self::from_exec(expr, schema)?),
                pattern: Box::new(Self::from_exec(pattern, schema)?),
                negated: *negated,
            }),
            ExecExpr::Case {
                operand,
                branches,
                else_expr,
            } => Ok(Self::Case {
                operand: operand
                    .as_deref()
                    .map(|expr| Self::from_exec(expr, schema))
                    .transpose()?
                    .map(Box::new),
                branches: branches
                    .iter()
                    .map(|(when, then)| {
                        Ok((
                            Self::from_exec(when, schema)?,
                            Self::from_exec(then, schema)?,
                        ))
                    })
                    .collect::<Result<Vec<_>, SqlError>>()?,
                else_expr: else_expr
                    .as_deref()
                    .map(|expr| Self::from_exec(expr, schema))
                    .transpose()?
                    .map(Box::new),
            }),
            ExecExpr::Binary { op, left, right } => Ok(Self::Binary {
                op: *op,
                left: Box::new(Self::from_exec(left, schema)?),
                right: Box::new(Self::from_exec(right, schema)?),
            }),
        }
    }

    fn write_sexpr(&self, out: &mut String) {
        match self {
            Self::Literal(value) => push_plan_value(out, value),
            Self::Column(name) => {
                out.push_str("(col ");
                push_string(out, name);
                out.push(')');
            }
            Self::IsNull { expr, negated } => {
                out.push('(');
                out.push_str(if *negated { "is-not-null" } else { "is-null" });
                out.push(' ');
                expr.write_sexpr(out);
                out.push(')');
            }
            Self::IsTruth {
                expr,
                expected,
                negated,
            } => {
                out.push('(');
                out.push_str(if *negated { "is-not-truth" } else { "is-truth" });
                out.push(' ');
                expr.write_sexpr(out);
                out.push(' ');
                out.push_str(truth_name(*expected));
                out.push(')');
            }
            Self::Not(expr) => {
                out.push_str("(not ");
                expr.write_sexpr(out);
                out.push(')');
            }
            Self::InList {
                expr,
                values,
                negated,
            } => {
                out.push('(');
                out.push_str(if *negated { "not-in" } else { "in" });
                out.push(' ');
                expr.write_sexpr(out);
                out.push_str(" (");
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        out.push(' ');
                    }
                    value.write_sexpr(out);
                }
                out.push_str("))");
            }
            Self::Between {
                expr,
                low,
                high,
                negated,
            } => {
                out.push('(');
                out.push_str(if *negated { "not-between" } else { "between" });
                out.push(' ');
                expr.write_sexpr(out);
                out.push(' ');
                low.write_sexpr(out);
                out.push(' ');
                high.write_sexpr(out);
                out.push(')');
            }
            Self::Like {
                expr,
                pattern,
                negated,
            } => {
                out.push('(');
                out.push_str(if *negated { "not-like" } else { "like" });
                out.push(' ');
                expr.write_sexpr(out);
                out.push(' ');
                pattern.write_sexpr(out);
                out.push(')');
            }
            Self::Case {
                operand,
                branches,
                else_expr,
            } => {
                out.push_str("(case");
                if let Some(operand) = operand {
                    out.push_str(" :operand ");
                    operand.write_sexpr(out);
                }
                for (when, then) in branches {
                    out.push_str(" (when ");
                    when.write_sexpr(out);
                    out.push(' ');
                    then.write_sexpr(out);
                    out.push(')');
                }
                if let Some(else_expr) = else_expr {
                    out.push_str(" :else ");
                    else_expr.write_sexpr(out);
                }
                out.push(')');
            }
            Self::Binary { op, left, right } => {
                out.push('(');
                out.push_str(binary_name(*op));
                out.push(' ');
                left.write_sexpr(out);
                out.push(' ');
                right.write_sexpr(out);
                out.push(')');
            }
        }
    }
}

impl JoinPlan {
    /// Reconstruct the exact fragments historically appended to a join label.
    fn detail_fragments(&self) -> Vec<String> {
        let mut details = Vec::new();
        match &self.keys {
            Some(JoinKeys::Names(names)) if !names.is_empty() => {
                details.push(format!("keys=({})", names.join(", ")));
            }
            Some(JoinKeys::Pairs(pairs)) => {
                let keys = pairs
                    .iter()
                    .map(|key| format!("{} = {}", key.left, key.right))
                    .collect::<Vec<_>>()
                    .join(", ");
                details.push(format!("keys=({keys})"));
            }
            _ => {}
        }
        if self.residual {
            details.push("residual=yes".to_string());
        }
        if self.on_expr {
            details.push("on=expr".to_string());
        }
        details
    }
}

fn push_metrics(out: &mut String, metrics: &Metrics, unit: &str) {
    if let Some(actual) = metrics.actual {
        out.push_str(&format!(" actual {unit}={actual}"));
        if let Some(ms) = metrics.actual_ms {
            out.push_str(&format!(" time={ms:.3}ms"));
        }
    } else if let Some(est) = metrics.est {
        out.push_str(&format!(" est {unit}={est}"));
    }
}

fn push_sexpr_metrics(out: &mut String, metrics: &Metrics, unit: &str) {
    if let Some(actual) = metrics.actual {
        out.push_str(&format!(" :actual-{unit} {actual}"));
        if let Some(ms) = metrics.actual_ms {
            out.push_str(&format!(" :time-ms {ms:.3}"));
        }
    } else if let Some(est) = metrics.est {
        out.push_str(&format!(" :est-{unit} {est}"));
    }
}

fn push_plan_value(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Int(value) => out.push_str(&value.to_string()),
        Value::Text(value) => push_string(out, value),
        Value::Bool(true) => out.push_str("#t"),
        Value::Bool(false) => out.push_str("#f"),
    }
}

fn truth_name(value: TruthValue) -> &'static str {
    match value {
        TruthValue::True => "true",
        TruthValue::False => "false",
        TruthValue::Unknown => "unknown",
    }
}

fn binary_name(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::And => "and",
        BinaryOp::Or => "or",
        BinaryOp::Eq => "=",
        BinaryOp::Ne => "<>",
        BinaryOp::Lt => "<",
        BinaryOp::Le => "<=",
        BinaryOp::Gt => ">",
        BinaryOp::Ge => ">=",
        BinaryOp::Add => "+",
        BinaryOp::Sub => "-",
        BinaryOp::Mul => "*",
        BinaryOp::Div => "/",
    }
}

fn push_string(out: &mut String, value: &str) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
}

fn push_string_list(out: &mut String, values: &[String]) {
    out.push('(');
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            out.push(' ');
        }
        push_string(out, value);
    }
    out.push(')');
}

fn push_join_keys(out: &mut String, keys: &JoinKeys) {
    match keys {
        JoinKeys::Names(names) => push_string_list(out, names),
        JoinKeys::Pairs(pairs) => {
            out.push('(');
            for (index, key) in pairs.iter().enumerate() {
                if index > 0 {
                    out.push(' ');
                }
                out.push('(');
                push_string(out, &key.left);
                out.push(' ');
                push_string(out, &key.right);
                out.push(')');
            }
            out.push(')');
        }
    }
}

fn join_kind_name(kind: JoinKind) -> &'static str {
    match kind {
        JoinKind::Inner => "inner",
        JoinKind::Cross => "cross",
        JoinKind::Left => "left",
        JoinKind::Right => "right",
        JoinKind::Full => "full",
    }
}

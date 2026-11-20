//! Structured representation of the text emitted by `EXPLAIN`.
//!
//! The tree is deliberately small: it captures the operators and the
//! observability fields the executor already exposes, without pretending to be
//! a full logical or physical plan. `to_text` preserves the historical
//! `EXPLAIN` text byte-for-byte; `to_sexpr` is a read-only, data-oriented view
//! intended for debugging and future rule matching.

use crate::ast::JoinKind;

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
    Filter(Metrics),
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

/// Join-specific fields. `details` contains the already-rendered key/residual
/// fragments so the text form remains stable while the tree itself is data.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct JoinPlan {
    pub(crate) uses_hash: bool,
    pub(crate) kind: JoinKind,
    pub(crate) details: Vec<String>,
    pub(crate) metrics: Metrics,
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
                if !join.details.is_empty() {
                    text.push(' ');
                    text.push_str(&join.details.join(" "));
                }
                push_metrics(&mut text, &join.metrics, "rows");
                text
            }
            PlanKind::Filter(metrics) => {
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
                if !join.details.is_empty() {
                    out.push_str(" :details ");
                    push_string_list(out, &join.details);
                }
                push_sexpr_metrics(out, &join.metrics, "rows");
                self.write_sexpr_children(out, indent);
                out.push(')');
            }
            PlanKind::Filter(metrics) => {
                out.push_str("(filter");
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

fn join_kind_name(kind: JoinKind) -> &'static str {
    match kind {
        JoinKind::Inner => "inner",
        JoinKind::Cross => "cross",
        JoinKind::Left => "left",
        JoinKind::Right => "right",
        JoinKind::Full => "full",
    }
}

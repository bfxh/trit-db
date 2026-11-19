use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::str;

use tritdb_exec::{
    compare_values, eval, truth_of, Column, ExecError, Expr as ExecExpr, Row, Schema, Table,
    TruthValue, Value,
};
use tritdb_txn::TxnStore;

use crate::ast::{AggregateKind, Assignment, JoinKind, Select, Statement};
use crate::bind::{bind_expr, bind_select, eval_constant, BoundSelect, GroupExpr};
use crate::error::SqlError;
use crate::parser::parse_script;
use crate::storage::{decode_table, encode_table, table_key, TABLE_KEY_PREFIX};

/// Result of one SQL statement.
#[derive(Clone, Debug, PartialEq)]
pub enum QueryResult {
    /// Rows with column names.
    Rows {
        /// Output column names.
        columns: Vec<String>,
        /// Output rows.
        rows: Vec<Vec<Value>>,
    },
    /// DDL/DML acknowledgement.
    Message(String),
}

/// A SQL engine backed by one [`TxnStore`].
///
/// M1 keeps the whole table in memory and rewrites the table value on every
/// mutation. This is a semantic spike, not a buffer pool or a heap file.
pub struct SqlEngine {
    store: TxnStore,
    tables: BTreeMap<String, Table>,
}

impl SqlEngine {
    /// Opens an engine and loads all persisted tables.
    pub fn open(path: &Path) -> Result<Self, SqlError> {
        let store = TxnStore::open(path)?;
        let mut tables = BTreeMap::new();
        for (key, value) in store.iter()? {
            let Some(name_bytes) = key.strip_prefix(TABLE_KEY_PREFIX.as_bytes()) else {
                continue;
            };
            let name = str::from_utf8(name_bytes).map_err(|error| SqlError::CorruptTable {
                table: "<non-utf8>".to_string(),
                reason: error.to_string(),
            })?;
            if name.is_empty() {
                return Err(SqlError::CorruptTable {
                    table: "<empty>".to_string(),
                    reason: "empty table name".to_string(),
                });
            }
            let table = decode_table(name, &value)?;
            tables.insert(name.to_string(), table);
        }
        Ok(Self { store, tables })
    }

    /// WAL path.
    pub fn path(&self) -> &Path {
        self.store.path()
    }

    /// Executes a SQL script and returns one result per statement.
    pub fn execute_script(&mut self, sql: &str) -> Result<Vec<QueryResult>, SqlError> {
        let statements = parse_script(sql)?;
        let mut results = Vec::with_capacity(statements.len());
        for statement in statements {
            results.push(self.execute_statement(&statement)?);
        }
        Ok(results)
    }

    /// Executes one parsed statement.
    pub fn execute_statement(&mut self, statement: &Statement) -> Result<QueryResult, SqlError> {
        match statement {
            Statement::CreateTable {
                name,
                if_not_exists,
                columns,
            } => self.create_table(name, *if_not_exists, columns),
            Statement::DropTable { name, if_exists } => self.drop_table(name, *if_exists),
            Statement::Insert {
                table,
                columns,
                rows,
            } => self.insert(table, columns.as_deref(), rows),
            Statement::Select(select) => self.select(select),
            Statement::Delete { table, filter } => self.delete(table, filter.as_ref()),
            Statement::Update {
                table,
                assignments,
                filter,
            } => self.update(table, assignments, filter.as_ref()),
        }
    }

    fn create_table(
        &mut self,
        name: &str,
        if_not_exists: bool,
        definitions: &[crate::ast::ColumnDef],
    ) -> Result<QueryResult, SqlError> {
        if self.tables.contains_key(name) {
            if if_not_exists {
                return Ok(QueryResult::Message(format!("table {name} already exists")));
            }
            return Err(SqlError::TableExists(name.to_string()));
        }
        if definitions.is_empty() {
            return Err(SqlError::Unsupported(
                "table must have at least one column".to_string(),
            ));
        }
        let mut columns = Vec::with_capacity(definitions.len());
        let mut seen = BTreeMap::new();
        for definition in definitions {
            if seen.insert(definition.name.clone(), ()).is_some() {
                return Err(SqlError::DuplicateColumn(definition.name.clone()));
            }
            columns.push(Column {
                name: definition.name.clone(),
                data_type: definition.data_type,
                not_null: definition.not_null,
            });
        }
        let table = Table {
            name: name.to_string(),
            schema: Schema::new(columns),
            rows: Vec::new(),
        };
        self.put_table(table)?;
        Ok(QueryResult::Message(format!("created table {name}")))
    }

    fn drop_table(&mut self, name: &str, if_exists: bool) -> Result<QueryResult, SqlError> {
        if !self.tables.contains_key(name) {
            if if_exists {
                return Ok(QueryResult::Message(format!("table {name} does not exist")));
            }
            return Err(SqlError::UnknownTable(name.to_string()));
        }
        let key = table_key(name);
        let mut txn = self.store.begin()?;
        txn.delete(&key)?;
        txn.commit()?;
        self.tables.remove(name);
        Ok(QueryResult::Message(format!("dropped table {name}")))
    }

    fn insert(
        &mut self,
        name: &str,
        target_columns: Option<&[String]>,
        rows: &[Vec<crate::ast::Expr>],
    ) -> Result<QueryResult, SqlError> {
        let mut table = self
            .tables
            .get(name)
            .cloned()
            .ok_or_else(|| SqlError::UnknownTable(name.to_string()))?;
        let schema = &table.schema;
        let indices = match target_columns {
            Some(names) => {
                let mut indices = Vec::with_capacity(names.len());
                let mut seen = BTreeMap::new();
                for column_name in names {
                    if seen.insert(column_name.clone(), ()).is_some() {
                        return Err(SqlError::DuplicateColumn(column_name.clone()));
                    }
                    let index = schema
                        .index_of(column_name)
                        .ok_or_else(|| SqlError::UnknownColumn(column_name.clone()))?;
                    indices.push(index);
                }
                indices
            }
            None => (0..schema.len()).collect(),
        };

        let mut new_rows = Vec::with_capacity(rows.len());
        for expressions in rows {
            if expressions.len() != indices.len() {
                return Err(SqlError::ColumnCount {
                    table: name.to_string(),
                    expected: indices.len(),
                    actual: expressions.len(),
                });
            }
            let mut row = vec![Value::Null; schema.len()];
            for (expression, &index) in expressions.iter().zip(&indices) {
                let value = eval_constant(expression)?;
                let value = schema.columns[index].data_type.coerce(value)?;
                if schema.columns[index].not_null && value.is_null() {
                    return Err(SqlError::NotNull {
                        table: name.to_string(),
                        column: schema.columns[index].name.clone(),
                    });
                }
                row[index] = value;
            }
            for (index, value) in row.iter().enumerate() {
                if value.is_null() && schema.columns[index].not_null {
                    return Err(SqlError::NotNull {
                        table: name.to_string(),
                        column: schema.columns[index].name.clone(),
                    });
                }
            }
            new_rows.push(row);
        }

        let inserted = new_rows.len();
        table.rows.extend(new_rows);
        self.put_table(table)?;
        Ok(QueryResult::Message(format!(
            "inserted {inserted} row(s) into {name}"
        )))
    }

    fn delete(
        &mut self,
        name: &str,
        filter: Option<&crate::ast::Expr>,
    ) -> Result<QueryResult, SqlError> {
        let mut table = self
            .tables
            .get(name)
            .cloned()
            .ok_or_else(|| SqlError::UnknownTable(name.to_string()))?;
        let filter = filter
            .map(|expr| bind_expr(expr, &table.schema))
            .transpose()?;
        let mut kept = Vec::with_capacity(table.rows.len());
        let mut deleted = 0usize;
        for row in table.rows.drain(..) {
            let keep = match &filter {
                Some(expr) => truth_of(&eval(expr, &table.schema, &row)?)? != TruthValue::True,
                None => false,
            };
            if keep {
                kept.push(row);
            } else {
                deleted += 1;
            }
        }
        table.rows = kept;
        if deleted > 0 {
            self.put_table(table)?;
        }
        Ok(QueryResult::Message(format!(
            "deleted {deleted} row(s) from {name}"
        )))
    }

    fn update(
        &mut self,
        name: &str,
        assignments: &[Assignment],
        filter: Option<&crate::ast::Expr>,
    ) -> Result<QueryResult, SqlError> {
        let mut table = self
            .tables
            .get(name)
            .cloned()
            .ok_or_else(|| SqlError::UnknownTable(name.to_string()))?;
        let schema = &table.schema;
        let mut targets = Vec::with_capacity(assignments.len());
        let mut seen = BTreeMap::new();
        for assignment in assignments {
            if seen.insert(assignment.column.clone(), ()).is_some() {
                return Err(SqlError::DuplicateColumn(assignment.column.clone()));
            }
            let index = schema
                .index_of(&assignment.column)
                .ok_or_else(|| SqlError::UnknownColumn(assignment.column.clone()))?;
            targets.push((index, bind_expr(&assignment.value, schema)?));
        }
        let filter = filter.map(|expr| bind_expr(expr, schema)).transpose()?;

        let mut updated = 0usize;
        for row in &mut table.rows {
            if !passes_filter(filter.as_ref(), schema, row)? {
                continue;
            }
            let old = row.clone();
            let mut values = Vec::with_capacity(targets.len());
            for (index, expr) in &targets {
                let value = eval(expr, schema, &old)?;
                let value = schema.columns[*index].data_type.coerce(value)?;
                if schema.columns[*index].not_null && value.is_null() {
                    return Err(SqlError::NotNull {
                        table: name.to_string(),
                        column: schema.columns[*index].name.clone(),
                    });
                }
                values.push((*index, value));
            }
            for (index, value) in values {
                row[index] = value;
            }
            updated += 1;
        }
        if updated > 0 {
            self.put_table(table)?;
        }
        Ok(QueryResult::Message(format!(
            "updated {updated} row(s) in {name}"
        )))
    }

    fn select(&self, select: &Select) -> Result<QueryResult, SqlError> {
        let table = self.build_from(select)?;
        let plan = bind_select(select, &table)?;
        execute_select(&table, &plan)
    }

    fn build_from(&self, select: &Select) -> Result<Table, SqlError> {
        let table = self
            .tables
            .get(&select.table)
            .ok_or_else(|| SqlError::UnknownTable(select.table.clone()))?;
        let mut qualifiers = BTreeSet::new();
        let mut sources = Vec::with_capacity(select.joins.len() + 1);
        let left_qualifier = select.table_alias.as_deref().unwrap_or(&select.table);
        if !qualifiers.insert(left_qualifier.to_string()) {
            return Err(SqlError::DuplicateTableAlias(left_qualifier.to_string()));
        }
        sources.push((table, left_qualifier));
        for join in &select.joins {
            let right = self
                .tables
                .get(&join.table)
                .ok_or_else(|| SqlError::UnknownTable(join.table.clone()))?;
            let qualifier = join.alias.as_deref().unwrap_or(&join.table);
            if !qualifiers.insert(qualifier.to_string()) {
                return Err(SqlError::DuplicateTableAlias(qualifier.to_string()));
            }
            sources.push((right, qualifier));
        }

        let mut joined = qualify_table(table, left_qualifier);
        for (join, (right, qualifier)) in select.joins.iter().zip(sources.into_iter().skip(1)) {
            let right = qualify_table(right, qualifier);
            joined = join_tables(&joined, &right, &join.on, join.kind)?;
        }
        Ok(joined)
    }

    fn put_table(&mut self, table: Table) -> Result<(), SqlError> {
        let key = table_key(&table.name);
        let value = encode_table(&table)?;
        let mut txn = self.store.begin()?;
        txn.put(&key, &value)?;
        txn.commit()?;
        self.tables.insert(table.name.clone(), table);
        Ok(())
    }
}

fn qualify_table(table: &Table, qualifier: &str) -> Table {
    let columns = table
        .schema
        .columns
        .iter()
        .map(|column| Column {
            name: format!("{qualifier}.{}", column.name),
            data_type: column.data_type,
            not_null: column.not_null,
        })
        .collect();
    Table {
        name: table.name.clone(),
        schema: Schema::new(columns),
        rows: table.rows.clone(),
    }
}

fn join_tables(
    left: &Table,
    right: &Table,
    on: &crate::ast::Expr,
    kind: JoinKind,
) -> Result<Table, SqlError> {
    let mut columns = left.schema.columns.clone();
    let mut right_columns = right.schema.columns.clone();
    if kind == JoinKind::Left {
        for column in &mut right_columns {
            column.not_null = false;
        }
    }
    columns.extend(right_columns);
    let schema = Schema::new(columns);
    let predicate = bind_expr(on, &schema)?;
    let mut rows = Vec::new();
    for left_row in &left.rows {
        let mut matched = false;
        for right_row in &right.rows {
            let mut row = left_row.clone();
            row.extend(right_row.iter().cloned());
            if passes_filter(Some(&predicate), &schema, &row)? {
                matched = true;
                rows.push(row);
            }
        }
        if kind == JoinKind::Left && !matched {
            let mut row = left_row.clone();
            row.extend(std::iter::repeat_n(Value::Null, right.schema.columns.len()));
            rows.push(row);
        }
    }
    Ok(Table {
        name: format!("{}+{}", left.name, right.name),
        schema,
        rows,
    })
}

fn passes_filter(filter: Option<&ExecExpr>, schema: &Schema, row: &Row) -> Result<bool, SqlError> {
    let Some(filter) = filter else {
        return Ok(true);
    };
    Ok(truth_of(&eval(filter, schema, row)?)? == TruthValue::True)
}

fn execute_select(table: &Table, plan: &BoundSelect) -> Result<QueryResult, SqlError> {
    let filtered: Vec<Row> = table
        .rows
        .iter()
        .filter_map(
            |row| match passes_filter(plan.filter.as_ref(), &table.schema, row) {
                Ok(true) => Some(Ok(row.clone())),
                Ok(false) => None,
                Err(error) => Some(Err(error)),
            },
        )
        .collect::<Result<Vec<_>, _>>()?;

    if plan.is_grouped() {
        return execute_grouped(table, plan, &filtered);
    }

    let mut rows = filtered;
    if !plan.order_by.is_empty() {
        let mut keyed = Vec::with_capacity(rows.len());
        for row in rows {
            let keys = {
                let group = Group { rows: vec![&row] };
                plan.order_by
                    .iter()
                    .map(|order| eval_group_expr(&order.expr, &table.schema, &group))
                    .collect::<Result<Vec<_>, _>>()?
            };
            keyed.push((keys, row));
        }
        validate_sort_types(&keyed, plan.order_by.len())?;
        keyed.sort_by(|left, right| compare_keys(&left.0, &right.0, plan));
        rows = keyed.into_iter().map(|(_, row)| row).collect();
    }

    let columns = plan
        .items
        .iter()
        .map(|item| item.name().to_string())
        .collect::<Vec<_>>();
    let mut output = Vec::with_capacity(rows.len());
    for row in rows {
        let group = Group { rows: vec![&row] };
        let mut projected = Vec::with_capacity(plan.items.len());
        for item in &plan.items {
            projected.push(eval_group_expr(&item.expr, &table.schema, &group)?);
        }
        output.push(projected);
    }
    if let Some(limit) = plan.limit {
        output.truncate(limit);
    }
    Ok(QueryResult::Rows {
        columns,
        rows: output,
    })
}

fn execute_grouped(
    table: &Table,
    plan: &BoundSelect,
    rows: &[Row],
) -> Result<QueryResult, SqlError> {
    let columns = plan
        .items
        .iter()
        .map(|item| item.name().to_string())
        .collect::<Vec<_>>();

    let groups = if plan.group_by.is_empty() {
        vec![Group {
            rows: rows.iter().collect(),
        }]
    } else {
        let mut grouped: BTreeMap<Vec<GroupKeyPart>, Vec<&Row>> = BTreeMap::new();
        for row in rows {
            let mut key = Vec::with_capacity(plan.group_by.len());
            for expr in &plan.group_by {
                let value = eval(expr, &table.schema, row)?;
                key.push(GroupKeyPart::from(&value));
            }
            grouped.entry(key).or_default().push(row);
        }
        grouped.into_values().map(|rows| Group { rows }).collect()
    };

    let mut kept = Vec::new();
    for group in groups {
        if let Some(having) = &plan.having {
            let value = eval_group_expr(having, &table.schema, &group)?;
            if truth_of(&value)? != TruthValue::True {
                continue;
            }
        }
        kept.push(group);
    }

    let mut keyed = Vec::with_capacity(kept.len());
    for group in kept {
        let mut projected = Vec::with_capacity(plan.items.len());
        for item in &plan.items {
            projected.push(eval_group_expr(&item.expr, &table.schema, &group)?);
        }
        let mut keys = Vec::with_capacity(plan.order_by.len());
        for order in &plan.order_by {
            keys.push(eval_group_expr(&order.expr, &table.schema, &group)?);
        }
        keyed.push((keys, projected));
    }

    if !plan.order_by.is_empty() {
        validate_sort_types(&keyed, plan.order_by.len())?;
        keyed.sort_by(|left, right| compare_keys(&left.0, &right.0, plan));
    }

    let mut output: Vec<Vec<Value>> = keyed.into_iter().map(|(_, row)| row).collect();
    if let Some(limit) = plan.limit {
        output.truncate(limit);
    }
    Ok(QueryResult::Rows {
        columns,
        rows: output,
    })
}

/// A value used as part of a grouping key. `NULL` compares and groups equal.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum GroupKeyPart {
    Null,
    Int(i64),
    Text(String),
    Bool(bool),
}

impl From<&Value> for GroupKeyPart {
    fn from(value: &Value) -> Self {
        match value {
            Value::Null => GroupKeyPart::Null,
            Value::Int(value) => GroupKeyPart::Int(*value),
            Value::Text(value) => GroupKeyPart::Text(value.clone()),
            Value::Bool(value) => GroupKeyPart::Bool(*value),
        }
    }
}

/// A group of source rows. A global aggregate over an empty table has one
/// group with no rows.
struct Group<'a> {
    rows: Vec<&'a Row>,
}

impl Group<'_> {
    fn representative(&self) -> Option<&Row> {
        self.rows.first().copied()
    }
}

fn eval_group_expr(
    expr: &GroupExpr,
    schema: &Schema,
    group: &Group<'_>,
) -> Result<Value, SqlError> {
    match expr {
        GroupExpr::Scalar(expr) => match group.representative() {
            Some(row) => Ok(eval(expr, schema, row)?),
            None => Ok(eval(expr, &Schema::default(), &[])?),
        },
        GroupExpr::Aggregate { kind, input } => {
            eval_aggregate(*kind, input.as_ref(), schema, &group.rows)
        }
        GroupExpr::IsNull { expr, negated } => {
            let value = eval_group_expr(expr, schema, group)?;
            let is_null = value.is_null();
            Ok(Value::Bool(if *negated { !is_null } else { is_null }))
        }
        GroupExpr::IsTruth {
            expr,
            expected,
            negated,
        } => {
            let value = eval_group_expr(expr, schema, group)?;
            let truth = truth_of(&value)?;
            let matched = truth == *expected;
            Ok(Value::Bool(if *negated { !matched } else { matched }))
        }
        GroupExpr::Not(expr) => {
            let value = eval_group_expr(expr, schema, group)?;
            Ok(truth_of(&value)?.not().to_value())
        }
        GroupExpr::InList {
            expr,
            values,
            negated,
        } => {
            let probe = eval_group_expr(expr, schema, group)?;
            let list = values
                .iter()
                .map(|value| eval_group_expr(value, schema, group))
                .collect::<Result<Vec<_>, _>>()?;
            let bound = ExecExpr::InList {
                expr: Box::new(ExecExpr::Literal(probe)),
                values: list.into_iter().map(ExecExpr::Literal).collect(),
                negated: *negated,
            };
            Ok(eval(&bound, &Schema::default(), &[])?)
        }
        GroupExpr::Between {
            expr,
            low,
            high,
            negated,
        } => {
            let probe = eval_group_expr(expr, schema, group)?;
            let low = eval_group_expr(low, schema, group)?;
            let high = eval_group_expr(high, schema, group)?;
            let bound = ExecExpr::Between {
                expr: Box::new(ExecExpr::Literal(probe)),
                low: Box::new(ExecExpr::Literal(low)),
                high: Box::new(ExecExpr::Literal(high)),
                negated: *negated,
            };
            Ok(eval(&bound, &Schema::default(), &[])?)
        }
        GroupExpr::Like {
            expr,
            pattern,
            negated,
        } => {
            let probe = eval_group_expr(expr, schema, group)?;
            let pattern = eval_group_expr(pattern, schema, group)?;
            let bound = ExecExpr::Like {
                expr: Box::new(ExecExpr::Literal(probe)),
                pattern: Box::new(ExecExpr::Literal(pattern)),
                negated: *negated,
            };
            Ok(eval(&bound, &Schema::default(), &[])?)
        }
        GroupExpr::Binary { op, left, right } => {
            let left = eval_group_expr(left, schema, group)?;
            let right = eval_group_expr(right, schema, group)?;
            let expr = ExecExpr::Binary {
                op: *op,
                left: Box::new(ExecExpr::Literal(left)),
                right: Box::new(ExecExpr::Literal(right)),
            };
            Ok(eval(&expr, &Schema::default(), &[])?)
        }
    }
}

fn validate_sort_types(keyed: &[(Vec<Value>, Row)], count: usize) -> Result<(), SqlError> {
    for index in 0..count {
        let mut data_type: Option<&'static str> = None;
        for (keys, _) in keyed {
            let value = &keys[index];
            if value.is_null() {
                continue;
            }
            match data_type {
                None => data_type = Some(value.type_name()),
                Some(expected) if expected == value.type_name() => {}
                Some(expected) => {
                    return Err(SqlError::Exec(ExecError::TypeMismatch {
                        expected,
                        actual: value.type_name(),
                    }));
                }
            }
        }
    }
    Ok(())
}

fn compare_keys(left: &[Value], right: &[Value], plan: &BoundSelect) -> Ordering {
    for (index, order) in plan.order_by.iter().enumerate() {
        let ordering = match (&left[index], &right[index]) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Null, _) => Ordering::Less,
            (_, Value::Null) => Ordering::Greater,
            (left, right) => match compare_values(left, right) {
                Ok(Some(ordering)) => ordering,
                _ => Ordering::Equal,
            },
        };
        if ordering != Ordering::Equal {
            return if order.ascending {
                ordering
            } else {
                ordering.reverse()
            };
        }
    }
    Ordering::Equal
}

fn eval_aggregate(
    kind: AggregateKind,
    input: Option<&ExecExpr>,
    schema: &Schema,
    rows: &[&Row],
) -> Result<Value, SqlError> {
    match kind {
        AggregateKind::Count => match input {
            None => Ok(Value::Int(
                i64::try_from(rows.len()).map_err(|_| ExecError::IntegerOverflow)?,
            )),
            Some(expr) => {
                let mut count = 0i64;
                for row in rows {
                    if !eval(expr, schema, row)?.is_null() {
                        count = count.checked_add(1).ok_or(ExecError::IntegerOverflow)?;
                    }
                }
                Ok(Value::Int(count))
            }
        },
        AggregateKind::Sum => {
            let Some(expr) = input else {
                return Err(SqlError::Unsupported(
                    "SUM requires an argument".to_string(),
                ));
            };
            let mut sum: Option<i64> = None;
            for row in rows {
                match eval(expr, schema, row)? {
                    Value::Null => {}
                    Value::Int(value) => {
                        sum = Some(
                            sum.unwrap_or(0)
                                .checked_add(value)
                                .ok_or(ExecError::IntegerOverflow)?,
                        );
                    }
                    other => {
                        return Err(SqlError::Exec(ExecError::TypeMismatch {
                            expected: "INT",
                            actual: other.type_name(),
                        }));
                    }
                }
            }
            Ok(sum.map_or(Value::Null, Value::Int))
        }
        AggregateKind::Min | AggregateKind::Max => {
            let Some(expr) = input else {
                return Err(SqlError::Unsupported(format!(
                    "{} requires an argument",
                    kind.name()
                )));
            };
            let mut best: Option<Value> = None;
            for row in rows {
                let value = eval(expr, schema, row)?;
                if value.is_null() {
                    continue;
                }
                match &best {
                    None => best = Some(value),
                    Some(current) => {
                        let ordering = compare_values(&value, current)?.ok_or_else(|| {
                            SqlError::Exec(ExecError::TypeMismatch {
                                expected: current.type_name(),
                                actual: value.type_name(),
                            })
                        })?;
                        let replace = match kind {
                            AggregateKind::Min => ordering == Ordering::Less,
                            AggregateKind::Max => ordering == Ordering::Greater,
                            _ => false,
                        };
                        if replace {
                            best = Some(value);
                        }
                    }
                }
            }
            Ok(best.unwrap_or(Value::Null))
        }
    }
}

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::str;
use std::time::Instant;

use tritdb_exec::{
    compare_values, eval, truth_of, BinaryOp, Column, ExecError, Expr as ExecExpr, Row, Schema,
    Table, TruthValue, Value,
};
use tritdb_txn::TxnStore;

use crate::ast::{AggregateKind, Assignment, Join, JoinCondition, JoinKind, Select, Statement};
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

/// JOIN execution strategy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum JoinStrategy {
    /// Use hash join when equi-keys are available, otherwise nested loop.
    #[default]
    Auto,
    /// Prefer hash join when equi-keys are available.
    Hash,
    /// Always use nested loop, for benchmarks and differential tests.
    NestedLoop,
}

/// Runtime counters attached to an `EXPLAIN ANALYZE` plan.
#[derive(Debug, Default)]
struct ExecutionStats {
    scan_rows: Vec<usize>,
    join_rows: Vec<usize>,
    join_ms: Vec<f64>,
    filter_rows: usize,
    filter_ms: f64,
    aggregate_groups: usize,
    aggregate_ms: f64,
    having_groups: usize,
    having_ms: f64,
    project_rows: usize,
    project_ms: f64,
    sort_rows: usize,
    sort_ms: f64,
    output_rows: usize,
    total_ms: f64,
}

/// A SQL engine backed by one [`TxnStore`].
///
/// M1 keeps the whole table in memory and rewrites the table value on every
/// mutation. This is a semantic spike, not a buffer pool or a heap file.
pub struct SqlEngine {
    store: TxnStore,
    tables: BTreeMap<String, Table>,
    join_strategy: JoinStrategy,
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
        Ok(Self {
            store,
            tables,
            join_strategy: JoinStrategy::Auto,
        })
    }

    /// WAL path.
    pub fn path(&self) -> &Path {
        self.store.path()
    }

    /// Current JOIN strategy.
    pub const fn join_strategy(&self) -> JoinStrategy {
        self.join_strategy
    }

    /// Overrides the JOIN strategy for benchmarks or differential tests.
    pub fn set_join_strategy(&mut self, strategy: JoinStrategy) {
        self.join_strategy = strategy;
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
            Statement::Explain(select) => self.explain(select, false),
            Statement::ExplainAnalyze(select) => self.explain(select, true),
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

    fn explain(&self, select: &Select, analyze: bool) -> Result<QueryResult, SqlError> {
        let (plan, stats) = if analyze {
            let start = Instant::now();
            let mut stats = ExecutionStats::default();
            let table = self.build_from_inner(select, Some(&mut stats))?;
            let plan = bind_select(select, &table)?;
            let result = execute_select_inner(&table, &plan, Some(&mut stats))?;
            if let QueryResult::Rows { rows, .. } = &result {
                stats.output_rows = rows.len();
            }
            stats.total_ms = start.elapsed().as_secs_f64() * 1000.0;
            let plan = self.build_plan(select, Some(&stats))?;
            (plan, Some(stats))
        } else {
            (self.build_plan(select, None)?, None)
        };
        let mut plan = plan;
        if let Some(stats) = stats {
            plan.push_str(&format!(
                "\nExecution: rows={} time={:.3}ms",
                stats.output_rows, stats.total_ms
            ));
        }
        Ok(QueryResult::Rows {
            columns: vec!["plan".to_string()],
            rows: plan
                .lines()
                .map(|line| vec![Value::Text(line.to_string())])
                .collect(),
        })
    }

    fn source_tables<'a>(&'a self, select: &Select) -> Result<Vec<(&'a Table, String)>, SqlError> {
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
        sources.push((table, left_qualifier.to_string()));
        for join in &select.joins {
            let right = self
                .tables
                .get(&join.table)
                .ok_or_else(|| SqlError::UnknownTable(join.table.clone()))?;
            let qualifier = join.alias.as_deref().unwrap_or(&join.table);
            if !qualifiers.insert(qualifier.to_string()) {
                return Err(SqlError::DuplicateTableAlias(qualifier.to_string()));
            }
            sources.push((right, qualifier.to_string()));
        }
        Ok(sources)
    }

    fn build_from(&self, select: &Select) -> Result<Table, SqlError> {
        self.build_from_inner(select, None)
    }

    fn build_from_inner(
        &self,
        select: &Select,
        mut stats: Option<&mut ExecutionStats>,
    ) -> Result<Table, SqlError> {
        let mut sources = self.source_tables(select)?.into_iter();
        let (left, left_qualifier) = sources.next().expect("base table is always present");
        if let Some(stats) = stats.as_deref_mut() {
            stats.scan_rows.push(left.rows.len());
        }
        let mut joined = qualify_table(left, &left_qualifier);
        for (join, (right, qualifier)) in select.joins.iter().zip(sources) {
            if let Some(stats) = stats.as_deref_mut() {
                stats.scan_rows.push(right.rows.len());
            }
            let start = stats.is_some().then(Instant::now);
            let right = qualify_table(right, &qualifier);
            joined = join_tables(
                &joined,
                &right,
                &join.condition,
                join.kind,
                self.join_strategy,
            )?;
            if let (Some(stats), Some(start)) = (stats.as_deref_mut(), start) {
                stats.join_rows.push(joined.rows.len());
                stats.join_ms.push(start.elapsed().as_secs_f64() * 1000.0);
            }
        }
        Ok(joined)
    }

    fn build_plan(
        &self,
        select: &Select,
        stats: Option<&ExecutionStats>,
    ) -> Result<String, SqlError> {
        let mut sources = self.source_tables(select)?.into_iter();
        let (left, left_qualifier) = sources.next().expect("base table is always present");
        let mut left_table = qualify_table_schema(left, &left_qualifier);
        let mut scan_index = 0;
        let mut plan = scan_plan(
            &source_label(left.name.as_str(), &left_qualifier),
            stats,
            scan_index,
        );
        scan_index += 1;
        for (join_index, (join, (right, qualifier))) in select.joins.iter().zip(sources).enumerate()
        {
            let right_table = qualify_table_schema(right, &qualifier);
            let pairs = match &join.condition {
                JoinCondition::Using(names) => using_pairs(&left_table, &right_table, names)?,
                JoinCondition::Natural => natural_pairs(&left_table, &right_table)?,
                JoinCondition::On(_) | JoinCondition::None => Vec::new(),
            };
            let pair_schema = Schema::new(
                left_table
                    .schema
                    .columns
                    .iter()
                    .chain(right_table.schema.columns.iter())
                    .cloned()
                    .collect(),
            );
            let predicate = match &join.condition {
                JoinCondition::On(expr) => Some(bind_expr(expr, &pair_schema)?),
                JoinCondition::Using(_) | JoinCondition::Natural => {
                    using_predicate(&pairs, left_table.schema.len())
                }
                JoinCondition::None => None,
            };
            let hash_pairs = match &join.condition {
                JoinCondition::Using(_) | JoinCondition::Natural => pairs.clone(),
                JoinCondition::On(_) => predicate
                    .as_ref()
                    .map(|expr| equi_join_pairs(expr, left_table.schema.len()))
                    .unwrap_or_default(),
                JoinCondition::None => Vec::new(),
            };
            let uses_hash = use_hash_join(self.join_strategy, &hash_pairs);
            let mut header = join_plan_header(
                join,
                uses_hash,
                &hash_pairs,
                &left_table,
                &right_table,
                predicate.as_ref(),
            );
            if let Some(stats) = stats {
                if let (Some(rows), Some(ms)) = (
                    stats.join_rows.get(join_index),
                    stats.join_ms.get(join_index),
                ) {
                    header.push_str(&format!(" actual rows={rows} time={ms:.3}ms"));
                }
            }
            let right_plan = scan_plan(
                &source_label(right.name.as_str(), &qualifier),
                stats,
                scan_index,
            );
            scan_index += 1;
            plan = format!(
                "{header}\n{}\n{}",
                indent_plan(&plan, 2),
                indent_plan(&right_plan, 2)
            );
            let (schema, _) = build_join_layout(&left_table, &right_table, &pairs, join.kind);
            left_table = Table {
                name: left_table.name,
                schema,
                rows: Vec::new(),
            };
        }

        let bound = bind_select(select, &left_table)?;
        if bound.filter.is_some() {
            let label = stats.map_or_else(
                || "Filter".to_string(),
                |stats| {
                    format!(
                        "Filter actual rows={} time={:.3}ms",
                        stats.filter_rows, stats.filter_ms
                    )
                },
            );
            plan = format!("{label}\n{}", indent_plan(&plan, 2));
        }
        if bound.is_grouped() {
            let label = stats.map_or_else(
                || "Aggregate".to_string(),
                |stats| {
                    format!(
                        "Aggregate actual groups={} time={:.3}ms",
                        stats.aggregate_groups, stats.aggregate_ms
                    )
                },
            );
            plan = format!("{label}\n{}", indent_plan(&plan, 2));
            if bound.having.is_some() {
                let label = stats.map_or_else(
                    || "Having".to_string(),
                    |stats| {
                        format!(
                            "Having actual groups={} time={:.3}ms",
                            stats.having_groups, stats.having_ms
                        )
                    },
                );
                plan = format!("{label}\n{}", indent_plan(&plan, 2));
            }
        }
        let projection = bound
            .items
            .iter()
            .map(|item| item.name().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let label = stats.map_or_else(
            || format!("Project [{projection}]"),
            |stats| {
                format!(
                    "Project [{projection}] actual rows={} time={:.3}ms",
                    stats.project_rows, stats.project_ms
                )
            },
        );
        plan = format!("{label}\n{}", indent_plan(&plan, 2));
        if !bound.order_by.is_empty() {
            let label = stats.map_or_else(
                || format!("Sort ({} key(s))", bound.order_by.len()),
                |stats| {
                    format!(
                        "Sort ({} key(s)) actual rows={} time={:.3}ms",
                        bound.order_by.len(),
                        stats.sort_rows,
                        stats.sort_ms
                    )
                },
            );
            plan = format!("{label}\n{}", indent_plan(&plan, 2));
        }
        if let Some(limit) = bound.limit {
            let label = stats.map_or_else(
                || format!("Limit {limit}"),
                |stats| format!("Limit {limit} actual rows={}", stats.output_rows),
            );
            plan = format!("{label}\n{}", indent_plan(&plan, 2));
        }
        Ok(plan)
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
    let mut qualified = qualify_table_schema(table, qualifier);
    qualified.rows = table.rows.clone();
    qualified
}

fn qualify_table_schema(table: &Table, qualifier: &str) -> Table {
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
        rows: Vec::new(),
    }
}

#[derive(Clone, Copy, Debug)]
enum JoinSource {
    Left(usize),
    Right(usize),
    Coalesced { left: usize, right: usize },
}

fn unqualified_name(name: &str) -> &str {
    name.rsplit_once('.').map_or(name, |(_, column)| column)
}

fn find_join_column(schema: &Schema, name: &str) -> Result<usize, SqlError> {
    if let Some(index) = schema.index_of(name) {
        return Ok(index);
    }
    let mut matches = schema
        .columns
        .iter()
        .enumerate()
        .filter_map(|(index, column)| (unqualified_name(&column.name) == name).then_some(index));
    let Some(index) = matches.next() else {
        return Err(SqlError::UnknownColumn(name.to_string()));
    };
    if matches.next().is_some() {
        return Err(SqlError::AmbiguousColumn(name.to_string()));
    }
    Ok(index)
}

fn check_join_types(left: &Column, right: &Column) -> Result<(), SqlError> {
    if left.data_type != right.data_type {
        return Err(ExecError::TypeMismatch {
            expected: left.data_type.name(),
            actual: right.data_type.name(),
        }
        .into());
    }
    Ok(())
}

fn using_pairs(
    left: &Table,
    right: &Table,
    names: &[String],
) -> Result<Vec<(usize, usize)>, SqlError> {
    let mut seen = BTreeSet::new();
    let mut pairs = Vec::with_capacity(names.len());
    for name in names {
        if !seen.insert(name.clone()) {
            return Err(SqlError::Unsupported(format!(
                "duplicate USING column: {name}"
            )));
        }
        let left_index = find_join_column(&left.schema, name)?;
        let right_index = find_join_column(&right.schema, name)?;
        check_join_types(
            &left.schema.columns[left_index],
            &right.schema.columns[right_index],
        )?;
        pairs.push((left_index, right_index));
    }
    Ok(pairs)
}

fn natural_pairs(left: &Table, right: &Table) -> Result<Vec<(usize, usize)>, SqlError> {
    let mut seen = BTreeSet::new();
    let mut pairs = Vec::new();
    for (left_index, left_column) in left.schema.columns.iter().enumerate() {
        let name = unqualified_name(&left_column.name);
        if !seen.insert(name.to_string()) {
            return Err(SqlError::AmbiguousColumn(name.to_string()));
        }
        let right_index = match find_join_column(&right.schema, name) {
            Ok(index) => index,
            Err(SqlError::UnknownColumn(_)) => continue,
            Err(error) => return Err(error),
        };
        check_join_types(left_column, &right.schema.columns[right_index])?;
        pairs.push((left_index, right_index));
    }
    Ok(pairs)
}

fn build_join_layout(
    left: &Table,
    right: &Table,
    pairs: &[(usize, usize)],
    kind: JoinKind,
) -> (Schema, Vec<JoinSource>) {
    let mut columns = Vec::new();
    let mut sources = Vec::new();
    let mut aliases = BTreeMap::new();
    let mut used_left = BTreeSet::new();
    let mut used_right = BTreeSet::new();

    for &(left_index, right_index) in pairs {
        used_left.insert(left_index);
        used_right.insert(right_index);
        let left_column = &left.schema.columns[left_index];
        let right_column = &right.schema.columns[right_index];
        let name = unqualified_name(&left_column.name).to_string();
        let not_null = match kind {
            JoinKind::Inner => left_column.not_null || right_column.not_null,
            JoinKind::Left => left_column.not_null,
            JoinKind::Right => right_column.not_null,
            JoinKind::Full | JoinKind::Cross => false,
        };
        let output_index = columns.len();
        columns.push(Column {
            name: name.clone(),
            data_type: left_column.data_type,
            not_null,
        });
        sources.push(JoinSource::Coalesced {
            left: left_index,
            right: right_index,
        });
        aliases.insert(left_column.name.clone(), output_index);
        aliases.insert(right_column.name.clone(), output_index);
        for (alias, index) in left.schema.aliases() {
            if *index == left_index {
                aliases.insert(alias.clone(), output_index);
            }
        }
        for (alias, index) in right.schema.aliases() {
            if *index == right_index {
                aliases.insert(alias.clone(), output_index);
            }
        }
    }

    for (index, column) in left.schema.columns.iter().enumerate() {
        if used_left.contains(&index) {
            continue;
        }
        let not_null = match kind {
            JoinKind::Right | JoinKind::Full => false,
            _ => column.not_null,
        };
        let output_index = columns.len();
        columns.push(Column {
            name: column.name.clone(),
            data_type: column.data_type,
            not_null,
        });
        sources.push(JoinSource::Left(index));
        aliases.insert(column.name.clone(), output_index);
        for (alias, alias_index) in left.schema.aliases() {
            if *alias_index == index {
                aliases.insert(alias.clone(), output_index);
            }
        }
    }

    for (index, column) in right.schema.columns.iter().enumerate() {
        if used_right.contains(&index) {
            continue;
        }
        let not_null = match kind {
            JoinKind::Left | JoinKind::Full => false,
            _ => column.not_null,
        };
        let output_index = columns.len();
        columns.push(Column {
            name: column.name.clone(),
            data_type: column.data_type,
            not_null,
        });
        sources.push(JoinSource::Right(index));
        aliases.insert(column.name.clone(), output_index);
        for (alias, alias_index) in right.schema.aliases() {
            if *alias_index == index {
                aliases.insert(alias.clone(), output_index);
            }
        }
    }

    (Schema::new(columns).with_aliases(aliases), sources)
}

fn using_predicate(pairs: &[(usize, usize)], left_len: usize) -> Option<ExecExpr> {
    let mut expressions = pairs
        .iter()
        .map(|&(left_index, right_index)| ExecExpr::Binary {
            op: BinaryOp::Eq,
            left: Box::new(ExecExpr::Column(left_index)),
            right: Box::new(ExecExpr::Column(left_len + right_index)),
        });
    let first = expressions.next()?;
    Some(expressions.fold(first, |left, right| ExecExpr::Binary {
        op: BinaryOp::And,
        left: Box::new(left),
        right: Box::new(right),
    }))
}

fn project_join_row(left: &Row, right: &Row, sources: &[JoinSource]) -> Row {
    sources
        .iter()
        .map(|source| match source {
            JoinSource::Left(index) => left[*index].clone(),
            JoinSource::Right(index) => right[*index].clone(),
            JoinSource::Coalesced {
                left: left_index,
                right: right_index,
            } => {
                if left[*left_index].is_null() {
                    right[*right_index].clone()
                } else {
                    left[*left_index].clone()
                }
            }
        })
        .collect()
}

fn equi_pair(left: &ExecExpr, right: &ExecExpr, left_len: usize) -> Option<(usize, usize)> {
    match (left, right) {
        (ExecExpr::Column(left_index), ExecExpr::Column(right_index))
            if *left_index < left_len && *right_index >= left_len =>
        {
            Some((*left_index, *right_index - left_len))
        }
        (ExecExpr::Column(left_index), ExecExpr::Column(right_index))
            if *right_index < left_len && *left_index >= left_len =>
        {
            Some((*right_index, *left_index - left_len))
        }
        _ => None,
    }
}

fn collect_equi_pairs(expr: &ExecExpr, left_len: usize, pairs: &mut Vec<(usize, usize)>) {
    match expr {
        ExecExpr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => {
            collect_equi_pairs(left, left_len, pairs);
            collect_equi_pairs(right, left_len, pairs);
        }
        ExecExpr::Binary {
            op: BinaryOp::Eq,
            left,
            right,
        } => {
            if let Some(pair) = equi_pair(left, right, left_len) {
                if !pairs.contains(&pair) {
                    pairs.push(pair);
                }
            }
        }
        _ => {}
    }
}

fn equi_join_pairs(expr: &ExecExpr, left_len: usize) -> Vec<(usize, usize)> {
    let mut pairs = Vec::new();
    collect_equi_pairs(expr, left_len, &mut pairs);
    pairs
}

fn hash_key(row: &Row, keys: &[usize]) -> Option<Vec<Value>> {
    let mut key = Vec::with_capacity(keys.len());
    for &index in keys {
        if row[index].is_null() {
            return None;
        }
        key.push(row[index].clone());
    }
    Some(key)
}

fn hash_join_rows(
    left: &Table,
    right: &Table,
    sources: &[JoinSource],
    kind: JoinKind,
    predicate: Option<&ExecExpr>,
    pair_schema: &Schema,
    pairs: &[(usize, usize)],
) -> Result<Vec<Row>, SqlError> {
    let left_keys: Vec<usize> = pairs.iter().map(|&(left_index, _)| left_index).collect();
    let right_keys: Vec<usize> = pairs.iter().map(|&(_, right_index)| right_index).collect();
    let mut buckets: HashMap<Vec<Value>, Vec<usize>> = HashMap::new();
    for (index, right_row) in right.rows.iter().enumerate() {
        if let Some(key) = hash_key(right_row, &right_keys) {
            buckets.entry(key).or_default().push(index);
        }
    }

    let mut rows = Vec::new();
    let mut right_matched = vec![false; right.rows.len()];
    let null_left = vec![Value::Null; left.schema.len()];
    let null_right = vec![Value::Null; right.schema.len()];
    for left_row in &left.rows {
        let mut matched = false;
        let key = hash_key(left_row, &left_keys);
        if let Some(candidates) = key.as_ref().and_then(|key| buckets.get(key)) {
            for &index in candidates {
                let right_row = &right.rows[index];
                let mut pair = left_row.clone();
                pair.extend(right_row.iter().cloned());
                if passes_filter(predicate, pair_schema, &pair)? {
                    matched = true;
                    right_matched[index] = true;
                    rows.push(project_join_row(left_row, right_row, sources));
                }
            }
        }
        if matches!(kind, JoinKind::Left | JoinKind::Full) && !matched {
            rows.push(project_join_row(left_row, &null_right, sources));
        }
    }
    if matches!(kind, JoinKind::Right | JoinKind::Full) {
        for (index, right_row) in right.rows.iter().enumerate() {
            if right_matched[index] {
                continue;
            }
            rows.push(project_join_row(&null_left, right_row, sources));
        }
    }
    Ok(rows)
}

fn nested_loop_join_rows(
    left: &Table,
    right: &Table,
    sources: &[JoinSource],
    kind: JoinKind,
    predicate: Option<&ExecExpr>,
    pair_schema: &Schema,
) -> Result<Vec<Row>, SqlError> {
    let mut rows = Vec::new();
    let mut right_matched = vec![false; right.rows.len()];
    let null_left = vec![Value::Null; left.schema.len()];
    let null_right = vec![Value::Null; right.schema.len()];
    for left_row in &left.rows {
        let mut matched = false;
        for (index, right_row) in right.rows.iter().enumerate() {
            let mut pair = left_row.clone();
            pair.extend(right_row.iter().cloned());
            if passes_filter(predicate, pair_schema, &pair)? {
                matched = true;
                right_matched[index] = true;
                rows.push(project_join_row(left_row, right_row, sources));
            }
        }
        if matches!(kind, JoinKind::Left | JoinKind::Full) && !matched {
            rows.push(project_join_row(left_row, &null_right, sources));
        }
    }
    if matches!(kind, JoinKind::Right | JoinKind::Full) {
        for (index, right_row) in right.rows.iter().enumerate() {
            if right_matched[index] {
                continue;
            }
            rows.push(project_join_row(&null_left, right_row, sources));
        }
    }
    Ok(rows)
}

fn use_hash_join(strategy: JoinStrategy, pairs: &[(usize, usize)]) -> bool {
    match strategy {
        JoinStrategy::NestedLoop => false,
        JoinStrategy::Auto | JoinStrategy::Hash => !pairs.is_empty(),
    }
}

fn source_label(table: &str, qualifier: &str) -> String {
    if table == qualifier {
        table.to_string()
    } else {
        format!("{table} AS {qualifier}")
    }
}

fn scan_plan(label: &str, stats: Option<&ExecutionStats>, index: usize) -> String {
    let mut node = format!("Scan {label}");
    if let Some(rows) = stats.and_then(|stats| stats.scan_rows.get(index)) {
        node.push_str(&format!(" actual rows={rows}"));
    }
    node
}

fn indent_plan(plan: &str, spaces: usize) -> String {
    let prefix = " ".repeat(spaces);
    plan.lines()
        .map(|line| format!("{prefix}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
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

fn is_pure_equi_conjunction(expr: &ExecExpr) -> bool {
    match expr {
        ExecExpr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => is_pure_equi_conjunction(left) && is_pure_equi_conjunction(right),
        ExecExpr::Binary {
            op: BinaryOp::Eq,
            left,
            right,
        } => matches!(
            (left.as_ref(), right.as_ref()),
            (ExecExpr::Column(_), ExecExpr::Column(_))
        ),
        _ => false,
    }
}

fn join_plan_header(
    join: &Join,
    uses_hash: bool,
    hash_pairs: &[(usize, usize)],
    left: &Table,
    right: &Table,
    predicate: Option<&ExecExpr>,
) -> String {
    let mut header = format!(
        "{} ({})",
        if uses_hash { "HashJoin" } else { "NestedLoop" },
        join_kind_name(join.kind)
    );
    let mut details = Vec::new();
    match &join.condition {
        JoinCondition::Using(names) => {
            if !names.is_empty() {
                details.push(format!("keys=({})", names.join(", ")));
            }
        }
        JoinCondition::Natural => {
            if !hash_pairs.is_empty() {
                let names = hash_pairs
                    .iter()
                    .map(|(left_index, _)| unqualified_name(&left.schema.columns[*left_index].name))
                    .collect::<Vec<_>>()
                    .join(", ");
                details.push(format!("keys=({names})"));
            }
        }
        JoinCondition::On(_) => {
            if uses_hash {
                let keys = hash_pairs
                    .iter()
                    .map(|(left_index, right_index)| {
                        format!(
                            "{} = {}",
                            left.schema.columns[*left_index].name,
                            right.schema.columns[*right_index].name
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                details.push(format!("keys=({keys})"));
                if predicate.is_some_and(|expr| !is_pure_equi_conjunction(expr)) {
                    details.push("residual=yes".to_string());
                }
            } else if predicate.is_some() {
                details.push("on=expr".to_string());
            }
        }
        JoinCondition::None => {}
    }
    if !details.is_empty() {
        header.push(' ');
        header.push_str(&details.join(" "));
    }
    header
}

fn join_tables(
    left: &Table,
    right: &Table,
    condition: &JoinCondition,
    kind: JoinKind,
    strategy: JoinStrategy,
) -> Result<Table, SqlError> {
    let pairs = match condition {
        JoinCondition::Using(names) => using_pairs(left, right, names)?,
        JoinCondition::Natural => natural_pairs(left, right)?,
        JoinCondition::None | JoinCondition::On(_) => Vec::new(),
    };
    let (schema, sources) = build_join_layout(left, right, &pairs, kind);
    let pair_schema = Schema::new(
        left.schema
            .columns
            .iter()
            .chain(right.schema.columns.iter())
            .cloned()
            .collect(),
    );
    let predicate = match condition {
        JoinCondition::On(expr) => Some(bind_expr(expr, &pair_schema)?),
        JoinCondition::Using(_) | JoinCondition::Natural => {
            using_predicate(&pairs, left.schema.len())
        }
        JoinCondition::None => None,
    };
    let hash_pairs = match condition {
        JoinCondition::Using(_) | JoinCondition::Natural => pairs.clone(),
        JoinCondition::On(_) => predicate
            .as_ref()
            .map(|expr| equi_join_pairs(expr, left.schema.len()))
            .unwrap_or_default(),
        JoinCondition::None => Vec::new(),
    };
    let rows = if use_hash_join(strategy, &hash_pairs) {
        hash_join_rows(
            left,
            right,
            &sources,
            kind,
            predicate.as_ref(),
            &pair_schema,
            &hash_pairs,
        )?
    } else {
        nested_loop_join_rows(
            left,
            right,
            &sources,
            kind,
            predicate.as_ref(),
            &pair_schema,
        )?
    };
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
    execute_select_inner(table, plan, None)
}

fn execute_select_inner(
    table: &Table,
    plan: &BoundSelect,
    mut stats: Option<&mut ExecutionStats>,
) -> Result<QueryResult, SqlError> {
    let filter_start = stats.is_some().then(Instant::now);
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
    if let (Some(stats), Some(start)) = (stats.as_deref_mut(), filter_start) {
        stats.filter_rows = filtered.len();
        stats.filter_ms = start.elapsed().as_secs_f64() * 1000.0;
    }

    if plan.is_grouped() {
        return execute_grouped_inner(table, plan, &filtered, stats);
    }

    let mut rows = filtered;
    let sort_start = stats.is_some().then(Instant::now);
    let sort_rows = stats.is_some().then_some(rows.len());
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
    if let (Some(stats), Some(start), Some(rows)) = (stats.as_deref_mut(), sort_start, sort_rows) {
        stats.sort_rows = rows;
        stats.sort_ms = start.elapsed().as_secs_f64() * 1000.0;
    }

    let project_start = stats.is_some().then(Instant::now);
    let project_rows = rows.len();
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
    if let (Some(stats), Some(start)) = (stats.as_deref_mut(), project_start) {
        stats.project_rows = project_rows;
        stats.project_ms = start.elapsed().as_secs_f64() * 1000.0;
    }
    if let Some(limit) = plan.limit {
        output.truncate(limit);
    }
    if let Some(stats) = stats {
        stats.output_rows = output.len();
    }
    Ok(QueryResult::Rows {
        columns,
        rows: output,
    })
}

fn execute_grouped_inner(
    table: &Table,
    plan: &BoundSelect,
    rows: &[Row],
    mut stats: Option<&mut ExecutionStats>,
) -> Result<QueryResult, SqlError> {
    let columns = plan
        .items
        .iter()
        .map(|item| item.name().to_string())
        .collect::<Vec<_>>();

    let aggregate_start = stats.is_some().then(Instant::now);
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
    if let (Some(stats), Some(start)) = (stats.as_deref_mut(), aggregate_start) {
        stats.aggregate_groups = groups.len();
        stats.aggregate_ms = start.elapsed().as_secs_f64() * 1000.0;
    }

    let having_start = stats.is_some().then(Instant::now);
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
    if let (Some(stats), Some(start)) = (stats.as_deref_mut(), having_start) {
        stats.having_groups = kept.len();
        stats.having_ms = start.elapsed().as_secs_f64() * 1000.0;
    }

    let project_start = stats.is_some().then(Instant::now);
    let project_rows = kept.len();
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
    if let (Some(stats), Some(start)) = (stats.as_deref_mut(), project_start) {
        stats.project_rows = project_rows;
        stats.project_ms = start.elapsed().as_secs_f64() * 1000.0;
    }

    let sort_start = stats.is_some().then(Instant::now);
    let sort_rows = keyed.len();
    if !plan.order_by.is_empty() {
        validate_sort_types(&keyed, plan.order_by.len())?;
        keyed.sort_by(|left, right| compare_keys(&left.0, &right.0, plan));
    }
    if let (Some(stats), Some(start)) = (stats.as_deref_mut(), sort_start) {
        stats.sort_rows = sort_rows;
        stats.sort_ms = start.elapsed().as_secs_f64() * 1000.0;
    }

    let mut output: Vec<Vec<Value>> = keyed.into_iter().map(|(_, row)| row).collect();
    if let Some(limit) = plan.limit {
        output.truncate(limit);
    }
    if let Some(stats) = stats {
        stats.output_rows = output.len();
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
        GroupExpr::Case {
            operand,
            branches,
            else_expr,
        } => {
            if let Some(operand) = operand {
                let probe = eval_group_expr(operand, schema, group)?;
                for (when, then) in branches {
                    let candidate = eval_group_expr(when, schema, group)?;
                    if compare_values(&probe, &candidate)? == Some(Ordering::Equal) {
                        return eval_group_expr(then, schema, group);
                    }
                }
            } else {
                for (when, then) in branches {
                    let value = eval_group_expr(when, schema, group)?;
                    if truth_of(&value)? == TruthValue::True {
                        return eval_group_expr(then, schema, group);
                    }
                }
            }
            match else_expr {
                Some(expr) => eval_group_expr(expr, schema, group),
                None => Ok(Value::Null),
            }
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

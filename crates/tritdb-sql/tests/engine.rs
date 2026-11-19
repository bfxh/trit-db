use tritdb_exec::Value;
use tritdb_sql::{QueryResult, SqlEngine};
use tritdb_testkit::TempDir;

fn as_rows(result: &QueryResult) -> (&[String], &[Vec<Value>]) {
    match result {
        QueryResult::Rows { columns, rows } => (columns, rows),
        QueryResult::Message(message) => panic!("expected rows, got message: {message}"),
    }
}

fn message(result: &QueryResult) -> &str {
    match result {
        QueryResult::Message(message) => message,
        QueryResult::Rows { .. } => panic!("expected message, got rows"),
    }
}

#[test]
fn create_insert_select_and_reopen() {
    let dir = TempDir::new("sql-persist");
    let db = dir.join("sql.db");
    let mut engine = SqlEngine::open(&db).expect("open");
    let results = engine
        .execute_script(
            "CREATE TABLE t (id INT NOT NULL, name TEXT, flag BOOL); \
             INSERT INTO t VALUES (1, 'a', TRUE), (2, NULL, UNKNOWN), (3, 'c', FALSE); \
             SELECT id, name, flag FROM t WHERE flag IS NULL OR id >= 2 ORDER BY id DESC LIMIT 2;",
        )
        .expect("execute");
    assert_eq!(message(&results[0]), "created table t");
    assert_eq!(message(&results[1]), "inserted 3 row(s) into t");
    let (columns, rows) = as_rows(&results[2]);
    assert_eq!(columns, ["id", "name", "flag"]);
    assert_eq!(
        rows,
        [
            vec![
                Value::Int(3),
                Value::Text("c".to_string()),
                Value::Bool(false)
            ],
            vec![Value::Int(2), Value::Null, Value::Null],
        ]
    );
    drop(engine);

    let mut reopened = SqlEngine::open(&db).expect("reopen");
    let results = reopened
        .execute_script("SELECT id, name, flag FROM t ORDER BY id;")
        .expect("select");
    let (columns, rows) = as_rows(&results[0]);
    assert_eq!(columns, ["id", "name", "flag"]);
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0],
        vec![
            Value::Int(1),
            Value::Text("a".to_string()),
            Value::Bool(true)
        ]
    );
}

#[test]
fn three_valued_where_only_keeps_true() {
    let dir = TempDir::new("sql-3vl");
    let db = dir.join("sql.db");
    let mut engine = SqlEngine::open(&db).expect("open");
    engine
        .execute_script(
            "CREATE TABLE t (id INT, flag BOOL); \
             INSERT INTO t VALUES (1, TRUE), (2, FALSE), (3, NULL);",
        )
        .expect("seed");

    let true_rows = engine
        .execute_script("SELECT id FROM t WHERE flag ORDER BY id;")
        .expect("where true");
    assert_eq!(as_rows(&true_rows[0]).1, [vec![Value::Int(1)]]);

    let false_rows = engine
        .execute_script("SELECT id FROM t WHERE NOT flag ORDER BY id;")
        .expect("where not");
    assert_eq!(as_rows(&false_rows[0]).1, [vec![Value::Int(2)]]);

    let null_rows = engine
        .execute_script("SELECT id FROM t WHERE flag IS NULL ORDER BY id;")
        .expect("where null");
    assert_eq!(as_rows(&null_rows[0]).1, [vec![Value::Int(3)]]);

    let unknown_rows = engine
        .execute_script("SELECT id FROM t WHERE flag IS UNKNOWN ORDER BY id;")
        .expect("where unknown");
    assert_eq!(as_rows(&unknown_rows[0]).1, [vec![Value::Int(3)]]);

    let and_rows = engine
        .execute_script("SELECT id FROM t WHERE flag AND flag IS NULL ORDER BY id;")
        .expect("where and");
    assert!(as_rows(&and_rows[0]).1.is_empty());
}

#[test]
fn aggregates_ignore_null_and_count_star_counts_rows() {
    let dir = TempDir::new("sql-agg");
    let db = dir.join("sql.db");
    let mut engine = SqlEngine::open(&db).expect("open");
    engine
        .execute_script(
            "CREATE TABLE t (id INT NOT NULL, score INT); \
             INSERT INTO t VALUES (1, 10), (2, NULL), (3, 20), (4, NULL);",
        )
        .expect("seed");
    let results = engine
        .execute_script("SELECT COUNT(*), COUNT(score), SUM(score), MIN(score), MAX(score) FROM t;")
        .expect("aggregate");
    let (columns, rows) = as_rows(&results[0]);
    assert_eq!(
        columns,
        [
            "COUNT(*)",
            "COUNT(score)",
            "SUM(score)",
            "MIN(score)",
            "MAX(score)"
        ]
    );
    assert_eq!(
        rows[0],
        [
            Value::Int(4),
            Value::Int(2),
            Value::Int(30),
            Value::Int(10),
            Value::Int(20),
        ]
    );
}

#[test]
fn nulls_sort_first_ascending_and_last_descending() {
    let dir = TempDir::new("sql-order-null");
    let db = dir.join("sql.db");
    let mut engine = SqlEngine::open(&db).expect("open");
    engine
        .execute_script(
            "CREATE TABLE t (id INT, score INT); \
             INSERT INTO t VALUES (1, 2), (2, NULL), (3, 1);",
        )
        .expect("seed");
    let asc = engine
        .execute_script("SELECT id, score FROM t ORDER BY score ASC;")
        .expect("asc");
    assert_eq!(
        as_rows(&asc[0]).1,
        [
            vec![Value::Int(2), Value::Null],
            vec![Value::Int(3), Value::Int(1)],
            vec![Value::Int(1), Value::Int(2)],
        ]
    );
    let desc = engine
        .execute_script("SELECT id, score FROM t ORDER BY score DESC;")
        .expect("desc");
    assert_eq!(
        as_rows(&desc[0]).1,
        [
            vec![Value::Int(1), Value::Int(2)],
            vec![Value::Int(3), Value::Int(1)],
            vec![Value::Int(2), Value::Null],
        ]
    );
}

#[test]
fn delete_and_drop_persist() {
    let dir = TempDir::new("sql-delete");
    let db = dir.join("sql.db");
    let mut engine = SqlEngine::open(&db).expect("open");
    engine
        .execute_script(
            "CREATE TABLE t (id INT NOT NULL); \
             INSERT INTO t VALUES (1), (2), (3); \
             DELETE FROM t WHERE id >= 2;",
        )
        .expect("delete");
    let after_delete = engine.execute_script("SELECT id FROM t;").expect("select");
    assert_eq!(as_rows(&after_delete[0]).1, [vec![Value::Int(1)]]);
    drop(engine);

    let mut reopened = SqlEngine::open(&db).expect("reopen");
    let after_reopen = reopened
        .execute_script("SELECT id FROM t;")
        .expect("select");
    assert_eq!(as_rows(&after_reopen[0]).1, [vec![Value::Int(1)]]);
    let dropped = reopened.execute_script("DROP TABLE t;").expect("drop");
    assert_eq!(message(&dropped[0]), "dropped table t");
    assert!(reopened.execute_script("SELECT id FROM t;").is_err());
}

#[test]
fn not_null_and_type_checks_are_enforced() {
    let dir = TempDir::new("sql-constraints");
    let db = dir.join("sql.db");
    let mut engine = SqlEngine::open(&db).expect("open");
    let create = engine
        .execute_script("CREATE TABLE t (id INT NOT NULL, name TEXT);")
        .expect("create");
    assert_eq!(message(&create[0]), "created table t");
    assert!(engine
        .execute_script("INSERT INTO t VALUES (NULL, 'x');")
        .is_err());
    assert!(engine
        .execute_script("INSERT INTO t VALUES ('x', 'y');")
        .is_err());
    assert!(engine
        .execute_script("INSERT INTO t (id) VALUES (1), (1);")
        .is_ok());
}

#[test]
fn aliases_work_in_select_and_order_by() {
    let dir = TempDir::new("sql-alias");
    let db = dir.join("sql.db");
    let mut engine = SqlEngine::open(&db).expect("open");
    engine
        .execute_script(
            "CREATE TABLE t (id INT, score INT); \
             INSERT INTO t VALUES (2, 20), (1, 10), (3, 30);",
        )
        .expect("seed");
    let results = engine
        .execute_script("SELECT id AS k, score + 1 AS s FROM t ORDER BY s DESC LIMIT 2;")
        .expect("alias");
    let (columns, rows) = as_rows(&results[0]);
    assert_eq!(columns, ["k", "s"]);
    assert_eq!(
        rows,
        [
            vec![Value::Int(3), Value::Int(31)],
            vec![Value::Int(2), Value::Int(21)],
        ]
    );
}

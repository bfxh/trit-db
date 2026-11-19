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
fn group_by_counts_and_aggregates_per_key() {
    let dir = TempDir::new("sql-group");
    let db = dir.join("sql.db");
    let mut engine = SqlEngine::open(&db).expect("open");
    engine
        .execute_script(
            "CREATE TABLE t (id INT, score INT, name TEXT); \
             INSERT INTO t VALUES \
               (1, 10, 'a'), (2, 20, 'a'), (3, NULL, 'b'), \
               (4, 30, 'b'), (5, 10, NULL);",
        )
        .expect("seed");
    let results = engine
        .execute_script(
            "SELECT score, COUNT(*) AS n, SUM(id) AS total \
             FROM t GROUP BY score ORDER BY score;",
        )
        .expect("group");
    let (columns, rows) = as_rows(&results[0]);
    assert_eq!(columns, ["score", "n", "total"]);
    assert_eq!(
        rows,
        [
            vec![Value::Null, Value::Int(1), Value::Int(3)],
            vec![Value::Int(10), Value::Int(2), Value::Int(6)],
            vec![Value::Int(20), Value::Int(1), Value::Int(2)],
            vec![Value::Int(30), Value::Int(1), Value::Int(4)],
        ]
    );
}

#[test]
fn group_by_having_and_order_by_aggregate_alias() {
    let dir = TempDir::new("sql-group-having");
    let db = dir.join("sql.db");
    let mut engine = SqlEngine::open(&db).expect("open");
    engine
        .execute_script(
            "CREATE TABLE t (id INT, score INT, name TEXT); \
             INSERT INTO t VALUES \
               (1, 10, 'a'), (2, 20, 'a'), (3, NULL, 'b'), \
               (4, 30, 'b'), (5, 10, NULL);",
        )
        .expect("seed");
    let results = engine
        .execute_script(
            "SELECT name, COUNT(*) AS n, SUM(id) AS total \
             FROM t GROUP BY name HAVING COUNT(*) >= 2 \
             ORDER BY n DESC, name ASC;",
        )
        .expect("having");
    assert_eq!(
        as_rows(&results[0]).1,
        [
            vec![Value::Text("a".to_string()), Value::Int(2), Value::Int(3),],
            vec![Value::Text("b".to_string()), Value::Int(2), Value::Int(7),],
        ]
    );

    let scalar_having = engine
        .execute_script(
            "SELECT name, COUNT(*) FROM t GROUP BY name \
             HAVING name IS NOT NULL ORDER BY name;",
        )
        .expect("having column");
    assert_eq!(
        as_rows(&scalar_having[0]).1,
        [
            vec![Value::Text("a".to_string()), Value::Int(2)],
            vec![Value::Text("b".to_string()), Value::Int(2)],
        ]
    );
}

#[test]
fn group_by_without_aggregate_and_aggregate_order() {
    let dir = TempDir::new("sql-group-order");
    let db = dir.join("sql.db");
    let mut engine = SqlEngine::open(&db).expect("open");
    engine
        .execute_script(
            "CREATE TABLE t (id INT, score INT, name TEXT); \
             INSERT INTO t VALUES \
               (1, 10, 'a'), (2, 20, 'a'), (3, NULL, 'b'), \
               (4, 30, 'b'), (5, 10, NULL);",
        )
        .expect("seed");

    let groups = engine
        .execute_script("SELECT name FROM t GROUP BY name ORDER BY name;")
        .expect("group without aggregate");
    assert_eq!(
        as_rows(&groups[0]).1,
        [
            vec![Value::Null],
            vec![Value::Text("a".to_string())],
            vec![Value::Text("b".to_string())],
        ]
    );

    let ordered = engine
        .execute_script(
            "SELECT score AS s, COUNT(*) AS n FROM t \
             GROUP BY score ORDER BY n DESC, s ASC LIMIT 2;",
        )
        .expect("aggregate order");
    assert_eq!(
        as_rows(&ordered[0]).1,
        [
            vec![Value::Int(10), Value::Int(2)],
            vec![Value::Null, Value::Int(1)],
        ]
    );
}

#[test]
fn group_by_empty_input_and_global_aggregate() {
    let dir = TempDir::new("sql-group-empty");
    let db = dir.join("sql.db");
    let mut engine = SqlEngine::open(&db).expect("open");
    engine
        .execute_script("CREATE TABLE t (x INT);")
        .expect("create");
    let empty = engine
        .execute_script("SELECT COUNT(*), SUM(x) FROM t;")
        .expect("global aggregate");
    assert_eq!(as_rows(&empty[0]).1, [vec![Value::Int(0), Value::Null]]);

    let no_groups = engine
        .execute_script("SELECT x, COUNT(*) FROM t GROUP BY x;")
        .expect("empty groups");
    assert!(as_rows(&no_groups[0]).1.is_empty());
}

#[test]
fn group_by_rejects_ungrouped_columns_and_bad_having() {
    let dir = TempDir::new("sql-group-errors");
    let db = dir.join("sql.db");
    let mut engine = SqlEngine::open(&db).expect("open");
    engine
        .execute_script("CREATE TABLE t (id INT, score INT); INSERT INTO t VALUES (1, 10);")
        .expect("seed");

    let error = engine
        .execute_script("SELECT id, COUNT(*) FROM t GROUP BY score;")
        .expect_err("ungrouped column")
        .to_string();
    assert!(error.contains("must appear in GROUP BY"), "{error}");

    let error = engine
        .execute_script("SELECT id FROM t GROUP BY score + 1;")
        .expect_err("group expression")
        .to_string();
    assert!(
        error.contains("GROUP BY only supports column references"),
        "{error}"
    );

    let error = engine
        .execute_script("SELECT id FROM t HAVING id > 1;")
        .expect_err("having without group")
        .to_string();
    assert!(error.contains("HAVING requires GROUP BY"), "{error}");

    let error = engine
        .execute_script("SELECT * FROM t GROUP BY score;")
        .expect_err("wildcard with group")
        .to_string();
    assert!(error.contains("SELECT * with GROUP BY"), "{error}");
}

#[test]
fn inner_join_matches_rows_and_qualified_columns() {
    let dir = TempDir::new("sql-join");
    let db = dir.join("sql.db");
    let mut engine = SqlEngine::open(&db).expect("open");
    engine
        .execute_script(
            "CREATE TABLE users (id INT, name TEXT, dept INT); \
             CREATE TABLE depts (id INT, name TEXT); \
             INSERT INTO users VALUES (1, 'a', 10), (2, 'b', 20), (3, 'c', NULL), (4, 'd', 10); \
             INSERT INTO depts VALUES (10, 'eng'), (20, 'sales'), (30, 'ops');",
        )
        .expect("seed");
    let results = engine
        .execute_script(
            "SELECT u.id, u.name AS user_name, d.name AS dept_name \
             FROM users AS u JOIN depts AS d ON u.dept = d.id \
             ORDER BY u.id;",
        )
        .expect("join");
    let (columns, rows) = as_rows(&results[0]);
    assert_eq!(columns, ["id", "user_name", "dept_name"]);
    assert_eq!(
        rows,
        [
            vec![
                Value::Int(1),
                Value::Text("a".to_string()),
                Value::Text("eng".to_string()),
            ],
            vec![
                Value::Int(2),
                Value::Text("b".to_string()),
                Value::Text("sales".to_string()),
            ],
            vec![
                Value::Int(4),
                Value::Text("d".to_string()),
                Value::Text("eng".to_string()),
            ],
        ]
    );
}

#[test]
fn chained_join_matches_three_tables() {
    let dir = TempDir::new("sql-join-chain");
    let db = dir.join("sql.db");
    let mut engine = SqlEngine::open(&db).expect("open");
    engine
        .execute_script(
            "CREATE TABLE a (id INT NOT NULL, name TEXT); \
             CREATE TABLE b (id INT NOT NULL, aid INT, label TEXT); \
             CREATE TABLE c (id INT, bid INT, tag TEXT); \
             INSERT INTO a VALUES (1, 'one'), (2, 'two'); \
             INSERT INTO b VALUES (10, 1, 'x'), (20, 2, 'y'), (30, 2, 'z'); \
             INSERT INTO c VALUES (100, 10, 'p'), (200, 20, 'q');",
        )
        .expect("seed");

    let results = engine
        .execute_script(
            "SELECT x.name, y.label, z.tag FROM a AS x \
             JOIN b AS y ON x.id = y.aid \
             JOIN c AS z ON y.id = z.bid \
             ORDER BY x.name;",
        )
        .expect("chained join");
    let (columns, rows) = as_rows(&results[0]);
    assert_eq!(columns, ["name", "label", "tag"]);
    assert_eq!(
        rows,
        [
            vec![
                Value::Text("one".to_string()),
                Value::Text("x".to_string()),
                Value::Text("p".to_string()),
            ],
            vec![
                Value::Text("two".to_string()),
                Value::Text("y".to_string()),
                Value::Text("q".to_string()),
            ],
        ]
    );

    let filtered = engine
        .execute_script(
            "SELECT x.id FROM a AS x JOIN b AS y ON x.id = y.aid \
             JOIN c AS z ON y.id = z.bid AND x.id = 1;",
        )
        .expect("later on sees earlier tables");
    assert_eq!(as_rows(&filtered[0]).1, [vec![Value::Int(1)]]);
}

#[test]
fn left_join_preserves_unmatched_left_rows() {
    let dir = TempDir::new("sql-left-join");
    let db = dir.join("sql.db");
    let mut engine = SqlEngine::open(&db).expect("open");
    engine
        .execute_script(
            "CREATE TABLE users (id INT, name TEXT, dept INT); \
             CREATE TABLE depts (id INT, name TEXT); \
             INSERT INTO users VALUES (1, 'a', 10), (2, 'b', 20), (3, 'c', NULL), (4, 'd', 10); \
             INSERT INTO depts VALUES (10, 'eng'), (20, 'sales'), (30, 'ops');",
        )
        .expect("seed");

    let results = engine
        .execute_script(
            "SELECT u.id, d.name AS dept_name FROM users AS u \
             LEFT JOIN depts AS d ON u.dept = d.id ORDER BY u.id;",
        )
        .expect("left join");
    let (columns, rows) = as_rows(&results[0]);
    assert_eq!(columns, ["id", "dept_name"]);
    assert_eq!(
        rows,
        [
            vec![Value::Int(1), Value::Text("eng".to_string())],
            vec![Value::Int(2), Value::Text("sales".to_string())],
            vec![Value::Int(3), Value::Null],
            vec![Value::Int(4), Value::Text("eng".to_string())],
        ]
    );

    let outer = engine
        .execute_script(
            "SELECT u.id, d.name FROM users AS u \
             LEFT OUTER JOIN depts AS d ON u.dept = d.id ORDER BY u.id;",
        )
        .expect("left outer join");
    assert_eq!(as_rows(&outer[0]).1, rows);

    let count = engine
        .execute_script(
            "SELECT COUNT(*) FROM users AS u \
             LEFT JOIN depts AS d ON u.dept = d.id;",
        )
        .expect("left join count");
    assert_eq!(as_rows(&count[0]).1, [vec![Value::Int(4)]]);
}

#[test]
fn left_join_where_group_and_empty_right() {
    let dir = TempDir::new("sql-left-join-group");
    let db = dir.join("sql.db");
    let mut engine = SqlEngine::open(&db).expect("open");
    engine
        .execute_script(
            "CREATE TABLE users (id INT, name TEXT, dept INT); \
             CREATE TABLE depts (id INT, name TEXT); \
             CREATE TABLE empty (id INT, name TEXT); \
             INSERT INTO users VALUES (1, 'a', 10), (2, 'b', 20), (3, 'c', NULL), (4, 'd', 10); \
             INSERT INTO depts VALUES (10, 'eng'), (20, 'sales'), (30, 'ops');",
        )
        .expect("seed");

    let matched = engine
        .execute_script(
            "SELECT u.id, d.name FROM users AS u \
             LEFT JOIN depts AS d ON u.dept = d.id \
             WHERE d.name = 'eng' ORDER BY u.id;",
        )
        .expect("where matched");
    assert_eq!(
        as_rows(&matched[0]).1,
        [
            vec![Value::Int(1), Value::Text("eng".to_string())],
            vec![Value::Int(4), Value::Text("eng".to_string())],
        ]
    );

    let unmatched = engine
        .execute_script(
            "SELECT u.id FROM users AS u \
             LEFT JOIN depts AS d ON u.dept = d.id \
             WHERE d.name IS NULL ORDER BY u.id;",
        )
        .expect("where unmatched");
    assert_eq!(as_rows(&unmatched[0]).1, [vec![Value::Int(3)]]);

    let grouped = engine
        .execute_script(
            "SELECT d.name, COUNT(*) FROM users AS u \
             LEFT JOIN depts AS d ON u.dept = d.id \
             GROUP BY d.name ORDER BY d.name;",
        )
        .expect("group outer join");
    assert_eq!(
        as_rows(&grouped[0]).1,
        [
            vec![Value::Null, Value::Int(1)],
            vec![Value::Text("eng".to_string()), Value::Int(2)],
            vec![Value::Text("sales".to_string()), Value::Int(1)],
        ]
    );

    let empty = engine
        .execute_script(
            "SELECT COUNT(*), COUNT(e.id) FROM users AS u \
             LEFT JOIN empty AS e ON u.id = e.id;",
        )
        .expect("empty right");
    assert_eq!(as_rows(&empty[0]).1, [vec![Value::Int(4), Value::Int(0)]]);
}

#[test]
fn join_with_group_by_and_where() {
    let dir = TempDir::new("sql-join-group");
    let db = dir.join("sql.db");
    let mut engine = SqlEngine::open(&db).expect("open");
    engine
        .execute_script(
            "CREATE TABLE users (id INT, name TEXT, dept INT); \
             CREATE TABLE depts (id INT, name TEXT); \
             INSERT INTO users VALUES (1, 'a', 10), (2, 'b', 20), (3, 'c', NULL), (4, 'd', 10); \
             INSERT INTO depts VALUES (10, 'eng'), (20, 'sales'), (30, 'ops');",
        )
        .expect("seed");
    let results = engine
        .execute_script(
            "SELECT d.name, COUNT(*) AS n FROM users AS u JOIN depts AS d \
             ON u.dept = d.id GROUP BY d.name ORDER BY d.name;",
        )
        .expect("group join");
    assert_eq!(
        as_rows(&results[0]).1,
        [
            vec![Value::Text("eng".to_string()), Value::Int(2)],
            vec![Value::Text("sales".to_string()), Value::Int(1)],
        ]
    );

    let filtered = engine
        .execute_script(
            "SELECT users.id, depts.name FROM users JOIN depts \
             ON users.dept = depts.id WHERE depts.name = 'eng' ORDER BY users.id;",
        )
        .expect("where join");
    assert_eq!(
        as_rows(&filtered[0]).1,
        [
            vec![Value::Int(1), Value::Text("eng".to_string())],
            vec![Value::Int(4), Value::Text("eng".to_string())],
        ]
    );
}

#[test]
fn self_join_supports_compound_on_and_rejects_duplicate_alias() {
    let dir = TempDir::new("sql-self-join");
    let db = dir.join("sql.db");
    let mut engine = SqlEngine::open(&db).expect("open");
    engine
        .execute_script(
            "CREATE TABLE users (id INT, name TEXT, dept INT); \
             INSERT INTO users VALUES (1, 'a', 10), (2, 'b', 20), (3, 'c', NULL), (4, 'd', 10);",
        )
        .expect("seed");
    let results = engine
        .execute_script(
            "SELECT u1.id, u2.id FROM users AS u1 JOIN users AS u2 \
             ON u1.dept = u2.dept AND u1.id < u2.id ORDER BY u1.id, u2.id;",
        )
        .expect("self join");
    assert_eq!(as_rows(&results[0]).1, [vec![Value::Int(1), Value::Int(4)]]);

    let error = engine
        .execute_script("SELECT u.id FROM users AS u JOIN users AS u ON u.id = u.id;")
        .expect_err("duplicate alias")
        .to_string();
    assert!(error.contains("duplicate table name or alias"), "{error}");
}

#[test]
fn join_rejects_ambiguous_columns_and_unknown_tables() {
    let dir = TempDir::new("sql-join-errors");
    let db = dir.join("sql.db");
    let mut engine = SqlEngine::open(&db).expect("open");
    engine
        .execute_script(
            "CREATE TABLE a (id INT); \
             CREATE TABLE b (id INT); \
             INSERT INTO a VALUES (1); \
             INSERT INTO b VALUES (1);",
        )
        .expect("seed");

    let qualified = engine
        .execute_script("SELECT a.id FROM a ORDER BY a.id;")
        .expect("qualified single table");
    assert_eq!(as_rows(&qualified[0]).1, [vec![Value::Int(1)]]);

    let error = engine
        .execute_script("SELECT id FROM a JOIN b ON a.id = b.id;")
        .expect_err("ambiguous")
        .to_string();
    assert!(error.contains("ambiguous column: id"), "{error}");

    let error = engine
        .execute_script("SELECT a.id FROM a JOIN missing ON a.id = missing.id;")
        .expect_err("unknown table")
        .to_string();
    assert!(error.contains("unknown table: missing"), "{error}");

    let error = engine
        .execute_script("SELECT a.id FROM a JOIN b ON id = 1;")
        .expect_err("unqualified ambiguous on")
        .to_string();
    assert!(error.contains("ambiguous column: id"), "{error}");
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

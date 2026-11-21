//! End-to-end "hint → repair → success" loop.
//!
//! Each scenario submits a broken query, checks that the error carries a
//! SQLSTATE plus a hint that names the fix, applies the repair the hint points
//! at, and asserts the repaired query succeeds. The claim under test is that
//! the error surface is *actionable* (a caller can converge in one retry), not
//! merely descriptive.

use tritdb_sql::{SqlEngine, SqlError};
use tritdb_testkit::TempDir;

/// Backtick-quoted names inside a hint, in order.
fn quoted_names(hint: &str) -> Vec<String> {
    hint.split('`')
        .enumerate()
        .filter(|(index, _)| index % 2 == 1)
        .map(|(_, name)| name.to_string())
        .collect()
}

struct Scenario {
    name: &'static str,
    seed: &'static str,
    broken: &'static str,
    code: &'static str,
    hint_contains: &'static str,
    repair: fn(&str, &[String]) -> String,
}

fn scenarios() -> Vec<Scenario> {
    vec![
        Scenario {
            name: "column-typo",
            seed: "CREATE TABLE customer (id INT NOT NULL, status TEXT); \
                   INSERT INTO customer VALUES (1, 'ok');",
            broken: "SELECT stat FROM customer;",
            code: "42703",
            hint_contains: "status",
            repair: |broken, names| broken.replace("stat", names.last().expect("candidate")),
        },
        Scenario {
            name: "table-typo",
            seed: "CREATE TABLE customer (id INT NOT NULL, status TEXT);",
            broken: "SELECT * FROM custmer;",
            code: "42P01",
            hint_contains: "customer",
            repair: |broken, names| broken.replace("custmer", names.last().expect("candidate")),
        },
        Scenario {
            name: "missing-group-by",
            seed: "CREATE TABLE g (k INT NOT NULL, v INT); \
                   INSERT INTO g VALUES (1, 2);",
            broken: "SELECT k, v FROM g GROUP BY k;",
            code: "42803",
            hint_contains: "GROUP BY",
            repair: |broken, names| {
                broken.replace("GROUP BY k;", &format!("GROUP BY k, {};", names[0]))
            },
        },
        Scenario {
            name: "ambiguous-column",
            seed: "CREATE TABLE a (id INT NOT NULL); CREATE TABLE b (id INT NOT NULL); \
                   INSERT INTO a VALUES (1); INSERT INTO b VALUES (1);",
            broken: "SELECT id FROM a, b;",
            code: "42702",
            hint_contains: "qualify",
            repair: |_broken, names| format!("SELECT a.{} FROM a, b;", names[0]),
        },
        Scenario {
            name: "not-null",
            seed: "CREATE TABLE t (id INT NOT NULL);",
            broken: "INSERT INTO t VALUES (NULL);",
            code: "23502",
            hint_contains: "t.id",
            repair: |broken, _names| broken.replace("NULL", "1"),
        },
        Scenario {
            name: "division-by-zero",
            seed: "CREATE TABLE nums (n INT, d INT); INSERT INTO nums VALUES (1, 0);",
            broken: "SELECT n / d AS q FROM nums;",
            code: "22012",
            hint_contains: "divisor",
            repair: |_broken, _names| {
                "SELECT CASE WHEN d = 0 THEN NULL ELSE n / d END AS q FROM nums;".to_string()
            },
        },
    ]
}

#[test]
fn every_hint_repairs_its_query_in_one_retry() {
    let scenarios = scenarios();
    assert_eq!(scenarios.len(), 6);
    let mut repaired = 0;
    for (index, scenario) in scenarios.iter().enumerate() {
        let dir = TempDir::new(&format!("agent-retry-{index}"));
        let db = dir.join("db");
        let mut engine = SqlEngine::open(&db).expect("open");
        engine
            .execute_script(scenario.seed)
            .expect("scenario seed must succeed");

        // First round fails, with a SQLSTATE and a hint that names the fix.
        let error: SqlError = engine
            .execute_script(scenario.broken)
            .expect_err(&format!("{} must fail on the first round", scenario.name));
        assert_eq!(error.code(), scenario.code, "{}: {error:?}", scenario.name);
        let hint = error
            .hint()
            .unwrap_or_else(|| panic!("{} must carry a hint: {error:?}", scenario.name));
        assert!(
            hint.contains(scenario.hint_contains),
            "{} hint should mention {:?}: {hint}",
            scenario.name,
            scenario.hint_contains
        );

        // Apply the repair the hint points at, then retry once.
        let names = quoted_names(&hint);
        let fixed = (scenario.repair)(scenario.broken, &names);
        engine.execute_script(&fixed).unwrap_or_else(|error| {
            panic!(
                "{} repair failed after hint: {fixed}: {error}",
                scenario.name
            )
        });
        repaired += 1;
    }
    assert_eq!(
        repaired,
        scenarios.len(),
        "every hint must repair its query in one retry"
    );
}

#[test]
fn structured_errors_expose_the_same_fields_in_json_and_text() {
    let dir = TempDir::new("agent-retry-json");
    let db = dir.join("db");
    let mut engine = SqlEngine::open(&db).expect("open");
    engine
        .execute_script("CREATE TABLE customer (id INT NOT NULL, status TEXT);")
        .expect("seed");
    let error = engine
        .execute_script("SELECT stat FROM customer;")
        .expect_err("column typo");
    // Text form carries the SQLSTATE; JSON form is built from the same parts.
    let text = error.format_structured();
    assert!(text.contains("[SQLSTATE 42703]"), "{text}");
    assert!(text.contains("hint: "), "{text}");
    let names = quoted_names(error.hint().as_deref().unwrap_or_default());
    assert_eq!(names.last().map(String::as_str), Some("status"));
}

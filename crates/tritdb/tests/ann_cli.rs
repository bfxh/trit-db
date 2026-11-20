//! CLI smoke tests for annotations.

use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tritdb"))
        .args(args)
        .output()
        .expect("run tritdb CLI")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("utf8 stdout")
}

#[test]
fn models_cli_counts_exactly() {
    let output = run(&["models", "--vars", "3", "--expr", "a AND (NOT b OR c)"]);
    assert!(output.status.success(), "stderr: {:?}", output);
    assert!(stdout(&output).contains("models=3"));
}

#[test]
fn repair_cli_reports_contingent_and_impossible() {
    let output = run(&["repair", "--facts", "2", "--denial", "0,1", "--query", "0"]);
    assert!(output.status.success(), "stderr: {:?}", output);
    assert!(stdout(&output).contains("answer=contingent"));

    let output = run(&[
        "repair", "--facts", "3", "--denial", "0,1", "--denial", "1,2", "--query", "0,1",
    ]);
    assert!(output.status.success(), "stderr: {:?}", output);
    assert!(stdout(&output).contains("answer=impossible"));
}

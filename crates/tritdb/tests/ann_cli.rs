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

#[test]
fn graph_cli_reach_reports_three_values() {
    let output = run(&[
        "graph", "reach", "--nodes", "3", "--edges", "0-1,1-2", "--from", "0", "--to", "2",
    ]);
    assert!(output.status.success(), "stderr: {:?}", output);
    let text = stdout(&output);
    assert!(text.contains("answer=certain"), "out: {text}");
    assert!(text.contains("hops=2"), "out: {text}");
    assert!(text.contains("path=0,1,2"), "out: {text}");

    let output = run(&[
        "graph", "reach", "--nodes", "3", "--edges", "0-1,1-2", "--label", "1-2=U", "--from", "0",
        "--to", "2",
    ]);
    assert!(output.status.success(), "stderr: {:?}", output);
    assert!(stdout(&output).contains("answer=contingent"));

    let output = run(&[
        "graph", "reach", "--nodes", "3", "--edges", "0-1,1-2", "--label", "1-2=F", "--from", "0",
        "--to", "2",
    ]);
    assert!(output.status.success(), "stderr: {:?}", output);
    let text = stdout(&output);
    assert!(text.contains("answer=impossible"), "out: {text}");
    assert!(text.contains("hops=-"), "out: {text}");
    assert!(text.contains("path=-"), "out: {text}");
}

#[test]
fn graph_cli_neighbors_and_knn() {
    let one_hop = run(&[
        "graph",
        "neighbors",
        "--nodes",
        "4",
        "--edges",
        "0-1,0-2,1-3",
        "--node",
        "0",
        "--hops",
        "1",
    ]);
    assert!(one_hop.status.success(), "stderr: {:?}", one_hop);
    let text = stdout(&one_hop);
    assert!(text.contains("count=2"), "out: {text}");
    assert!(text.contains("nodes=1,2"), "out: {text}");

    let two_hops = run(&[
        "graph",
        "neighbors",
        "--nodes",
        "4",
        "--edges",
        "0-1,0-2,1-3",
        "--node",
        "0",
        "--hops",
        "2",
    ]);
    assert!(two_hops.status.success(), "stderr: {:?}", two_hops);
    let text = stdout(&two_hops);
    assert!(text.contains("count=3"), "out: {text}");
    assert!(text.contains("nodes=1,2,3"), "out: {text}");

    let knn = run(&[
        "graph",
        "knn",
        "--points",
        "0,0;1,0;2,0",
        "--labels",
        "T,U,F",
        "--query",
        "0,0",
        "--k",
        "2",
    ]);
    assert!(knn.status.success(), "stderr: {:?}", knn);
    let text = stdout(&knn);
    assert!(
        text.contains("label=TRUE rank=0 id=0 distance=0"),
        "out: {text}"
    );
    assert!(
        text.contains("label=UNKNOWN rank=0 id=1 distance=1"),
        "out: {text}"
    );
    assert!(
        text.contains("label=FALSE rank=0 id=2 distance=4"),
        "out: {text}"
    );
    assert!(
        text.contains("certain=1 uncertain=1 excluded=1"),
        "out: {text}"
    );
}

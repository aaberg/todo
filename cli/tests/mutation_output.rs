//! Regression tests for post-mutation command output.
//!
//! `todo add`, `edit`, `done`, `undone` and `remove` must print both the
//! confirmation message and the refreshed list of todos after mutating
//! state. These tests run the real CLI binary with an isolated HOME so a
//! future change cannot silently drop or stale the printed list.

use std::process::Command;

/// Run the todo binary with an isolated HOME and return (stdout, stderr).
fn run(home: &std::path::Path, args: &[&str]) -> (String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_todo"))
        .args(args)
        .env("HOME", home)
        .output()
        .expect("failed to run todo binary");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Create a temp HOME and add three todos, returning the path.
fn setup_with_todos(label: &str, count: usize) -> std::path::PathBuf {
    let home = std::env::temp_dir().join(format!(
        "todo-mutation-output-{label}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    for i in 1..=count {
        let (stdout, stderr) = run(&home, &["add", &format!("todo {i}")]);
        assert!(
            stderr.is_empty(),
            "add failed unexpectedly: {stderr} (stdout: {stdout})"
        );
    }
    home
}

fn assert_list_contains(home: &std::path::Path, stdout: &str, expected: &[&str]) {
    for text in expected {
        assert!(
            stdout.contains(text),
            "expected output to contain {text:?}; got:\n{stdout}\n(home: {})",
            home.display()
        );
    }
}

#[test]
fn edit_prints_confirmation_and_updated_list() {
    let home = setup_with_todos("edit", 2);
    let (stdout, stderr) = run(&home, &["edit", "1", "edited todo"]);
    assert!(
        stderr.is_empty(),
        "edit failed unexpectedly: {stderr} (stdout: {stdout})"
    );
    assert!(stdout.contains("Updated todo 1."), "got:\n{stdout}");
    // Old description gone, new description shown in the refreshed list.
    assert_list_contains(&home, &stdout, &["edited todo"]);
    assert!(
        !stdout.contains("[ ] 1 todo 1"),
        "old description should not appear as a pending list entry; got:\n{stdout}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn done_prints_confirmation_and_updated_list() {
    let home = setup_with_todos("done", 2);
    let (stdout, stderr) = run(&home, &["done", "2"]);
    assert!(
        stderr.is_empty(),
        "done failed unexpectedly: {stderr} (stdout: {stdout})"
    );
    assert!(stdout.contains("Marked todo 2 as done."), "got:\n{stdout}");
    // The refreshed list shows both todos; todo 2 must appear completed ("x").
    assert_list_contains(&home, &stdout, &["todo 1"]);
    assert!(
        stdout.contains("[x] 2 todo 2") || (stdout.contains("[x]") && stdout.contains("todo 2")),
        "expected completed todo 2 in the refreshed list; got:\n{stdout}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn undone_prints_confirmation_and_updated_list() {
    let home = setup_with_todos("undone", 2);
    run(&home, &["done", "1"]);
    // Completed todos sort to the end, so the completed todo is now ID 2.
    let (stdout, stderr) = run(&home, &["undone", "2"]);
    assert!(
        stderr.is_empty(),
        "undone failed unexpectedly: {stderr} (stdout: {stdout})"
    );
    assert!(
        stdout.contains("Marked todo 2 as not done."),
        "got:\n{stdout}"
    );
    // The refreshed list must show the todo back to pending: no completed entry.
    assert!(
        !stdout.contains("[x]"),
        "expected no completed todo in the refreshed list; got:\n{stdout}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn remove_prints_confirmation_and_updated_list() {
    let home = setup_with_todos("remove", 3);
    let (stdout, stderr) = run(&home, &["remove", "2"]);
    assert!(
        stderr.is_empty(),
        "remove failed unexpectedly: {stderr} (stdout: {stdout})"
    );
    assert!(stdout.contains("Removed todo 2."), "got:\n{stdout}");
    // Refreshed list shows the remaining todos but not the removed one.
    assert_list_contains(&home, &stdout, &["todo 1", "todo 3"]);
    assert!(
        !stdout.contains("todo 2\n"),
        "removed todo should not appear in the refreshed list; got:\n{stdout}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn remove_last_todo_prints_empty_list() {
    let home = setup_with_todos("remove-last", 1);
    let (stdout, stderr) = run(&home, &["remove", "1"]);
    assert!(
        stderr.is_empty(),
        "remove failed unexpectedly: {stderr} (stdout: {stdout})"
    );
    assert!(stdout.contains("Removed todo 1."), "got:\n{stdout}");
    assert!(
        stdout.contains("No todos."),
        "expected empty-list output after removing the last todo; got:\n{stdout}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn add_prints_confirmation_and_updated_list() {
    let home = setup_with_todos("add", 0);
    let (stdout, stderr) = run(&home, &["add", "first todo"]);
    assert!(
        stderr.is_empty(),
        "add failed unexpectedly: {stderr} (stdout: {stdout})"
    );
    assert!(stdout.contains("Added todo for"), "got:\n{stdout}");
    assert_list_contains(&home, &stdout, &["first todo"]);
    let _ = std::fs::remove_dir_all(&home);
}

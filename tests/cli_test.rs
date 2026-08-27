use predicates::prelude::*;

#[test]
fn test_cli_version() {
    assert_cmd::cargo::cargo_bin_cmd!("hyprloom")
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("hyprloom"));
}

#[test]
fn test_cli_help() {
    assert_cmd::cargo::cargo_bin_cmd!("hyprloom")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("save"))
        .stdout(predicate::str::contains("restore"))
        .stdout(predicate::str::contains("replace"))
        .stdout(predicate::str::contains("list"))
        .stdout(predicate::str::contains("delete"));
}

#[test]
fn test_cli_help_describes_reconciliation() {
    assert_cmd::cargo::cargo_bin_cmd!("hyprloom")
        .args(["restore", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("reconcile"));
}

#[test]
fn test_cli_list_empty() {
    let tmp = tempfile::tempdir().unwrap();
    assert_cmd::cargo::cargo_bin_cmd!("hyprloom")
        .arg("list")
        .env("XDG_DATA_HOME", tmp.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("No saved sessions"));
}

#[test]
fn test_cli_delete_nonexistent() {
    let tmp = tempfile::tempdir().unwrap();
    assert_cmd::cargo::cargo_bin_cmd!("hyprloom")
        .args(["delete", "nonexistent"])
        .env("XDG_DATA_HOME", tmp.path())
        .assert()
        .failure();
}

#[test]
fn test_cli_config_shows_paths() {
    assert_cmd::cargo::cargo_bin_cmd!("hyprloom")
        .arg("config")
        .assert()
        .success()
        .stdout(predicate::str::contains("Config path"))
        .stdout(predicate::str::contains("Sessions dir"));
}

#[test]
fn test_autosave_help() {
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("hyprloom");
    cmd.args(["autosave", "--help"]);
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("autosave"));
}

use assert_cmd::Command;

#[test]
fn test_help_exits_zero() {
    let mut cmd = Command::cargo_bin("konform").unwrap();
    cmd.arg("--help").assert().success();
}

#[test]
fn test_version_flag() {
    let mut cmd = Command::cargo_bin("konform").unwrap();
    cmd.arg("--version").assert().success();
}

#[test]
fn test_version_subcommand() {
    let mut cmd = Command::cargo_bin("konform").unwrap();
    cmd.arg("version").assert().success();
}

use assert_cmd::Command;
use predicates::prelude::*;

fn pml() -> Command {
    Command::cargo_bin("pml").expect("binary built")
}

#[test]
fn help_lists_the_main_commands() {
    pml()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("orders"))
        .stdout(predicate::str::contains("positions"))
        .stdout(predicate::str::contains("perps"))
        .stdout(predicate::str::contains("auth"));
}

#[test]
fn tpsl_set_requires_a_change() {
    pml()
        .args(["perps", "tpsl", "set", "--instrument", "BTC-USD"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("nothing to change"));
}

#[test]
fn tpsl_set_rejects_conflicting_flags() {
    pml()
        .args([
            "perps",
            "tpsl",
            "set",
            "--instrument",
            "BTC-USD",
            "--take-profit",
            "70000",
            "--clear-take-profit",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not both"));
}

#[test]
fn invalid_position_address_is_reported() {
    pml()
        .args([
            "positions",
            "--venue",
            "prediction",
            "--address",
            "not-an-address",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("valid Ethereum address"));
}

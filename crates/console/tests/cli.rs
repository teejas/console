//! End-to-end checks of the `console` binary's surface.

use assert_cmd::Command;
use predicates::prelude::*;

fn console() -> Command {
    let mut cmd = Command::cargo_bin("console").unwrap();
    // Isolate from the developer's real config and environment.
    cmd.env_remove("CONSOLE_CONFIG").env_remove("RUST_LOG");
    cmd
}

#[test]
fn help_lists_core_roles() {
    console()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("orchestrator"))
        .stdout(predicate::str::contains("executor"))
        .stdout(predicate::str::contains("acp"));
}

#[test]
fn config_show_prints_effective_toml() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config.toml");
    std::fs::write(&file, "[agent]\neffort = \"high\"\n").unwrap();
    console()
        .args(["config", "show", "--config"])
        .arg(&file)
        .env("CONSOLE_AGENT__INLINE_MAX_TOOL_CALLS", "5")
        .assert()
        .success()
        .stdout(predicate::str::contains("effort = \"high\""))
        .stdout(predicate::str::contains("inline_max_tool_calls = 5"))
        .stdout(predicate::str::contains("model = \"claude-opus-5-5\""));
}

#[test]
fn invalid_config_fails_with_the_bad_key() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config.toml");
    std::fs::write(&file, "[agent]\nmodle = \"typo\"\n").unwrap();
    console()
        .args(["config", "show", "--config"])
        .arg(&file)
        .assert()
        .failure()
        .stderr(predicate::str::contains("modle"));
}

#[test]
fn config_path_honours_env() {
    console()
        .args(["config", "path"])
        .env("CONSOLE_CONFIG", "/tmp/elsewhere.toml")
        .assert()
        .success()
        .stdout("/tmp/elsewhere.toml\n");
}

#[test]
fn unbuilt_commands_name_their_milestone() {
    let dir = tempfile::tempdir().unwrap();
    console()
        .arg("queue")
        .env("CONSOLE_CONFIG", dir.path().join("none.toml"))
        .assert()
        .code(2)
        .stderr(predicate::str::contains("M4"));
}

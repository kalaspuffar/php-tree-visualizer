//! End-to-end tests for the `php-tree-viz-collector` binary. Each test
//! spawns the compiled executable, passes arguments, and asserts on
//! exit code and stdout/stderr. These complement the unit tests
//! inside the `config` module (which cover validation in isolation).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_php-tree-viz-collector");

fn unique_tempdir(label: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "phptv-collector-cli-{}-{}-{}",
        std::process::id(),
        label,
        n,
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_config_in(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("collector.toml");
    std::fs::write(&path, body).unwrap();
    path
}

fn run_binary(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .output()
        .expect("failed to run the collector binary")
}

fn run_with_config(path: &Path) -> Output {
    Command::new(BIN)
        .arg("--config")
        .arg(path)
        .output()
        .expect("failed to run the collector binary")
}

#[test]
fn invocation_without_arguments_exits_two_with_one_stderr_line() {
    let out = run_binary(&[]);
    assert_eq!(out.status.code(), Some(2));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stdout.is_empty(), "stdout was: {stdout:?}");
    assert_eq!(stderr.lines().count(), 1, "stderr was: {stderr:?}");
    assert!(stderr.contains("--config"));
}

#[test]
fn invocation_with_unknown_flag_exits_two_and_names_the_flag() {
    let out = run_binary(&["--bogus"]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("--bogus"));
    assert_eq!(stderr.lines().count(), 1);
}

#[test]
fn invocation_with_nonexistent_config_path_exits_two_and_names_the_path() {
    let path = "/definitely/does/not/exist/collector.toml";
    let out = run_binary(&["--config", path]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains(path), "stderr: {stderr:?}");
    assert_eq!(stderr.lines().count(), 1);
}

// The previous `valid_config_exits_zero_with_one_redacted_stdout_line`
// and `example_file_loads_via_binary_after_placeholder_substitution`
// tests have moved to `tests/http_skeleton.rs`: now that loading a
// valid config also starts an HTTP server that blocks until SIGTERM,
// the run-to-completion shape no longer applies, and the equivalent
// coverage uses the `Collector` spawn-and-kill helper from the http
// integration tests.

#[test]
fn malformed_toml_exits_two_on_a_single_stderr_line() {
    let dir = unique_tempdir("malformed");
    let path = write_config_in(&dir, "this is = = = not toml");

    let out = run_with_config(&path);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert_eq!(
        stderr.lines().count(),
        1,
        "stderr should be one line; was: {stderr:?}"
    );
    assert!(stderr.contains(path.to_str().unwrap()));
}

#[test]
fn validation_failure_inside_a_well_formed_toml_exits_two() {
    // Well-formed TOML, but token is too short.
    let body = r#"
[server]
bind = "127.0.0.1:8088"

[auth]
token = "short"
session_salt = "SSSSSSSSSSSSSSSSSSSSSSSSSSSSSSSSSSSSSSSS"

[storage]
data_dir = "/var/lib/php-tree-viz"
retention_days = 30
"#;
    let dir = unique_tempdir("short_token");
    let path = write_config_in(&dir, body);

    let out = run_with_config(&path);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("auth.token"));
    assert_eq!(stderr.lines().count(), 1);
}

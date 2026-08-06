use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn repository_root() -> PathBuf {
    std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap()
}

fn temporary_data(name: &str) -> PathBuf {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    std::env::temp_dir().join(format!("needle-explore-{name}-{nonce}"))
}

#[test]
fn direct_explore_requires_an_enabled_repository_without_creating_state() {
    let data = temporary_data("disabled");
    let output = Command::new(env!("CARGO_BIN_EXE_needle"))
        .args([
            "explore",
            "--route",
            "trace.state-flow",
            "--subject-kind",
            "behavior",
            "--subject",
            "activation",
            "--repository",
        ])
        .arg(repository_root())
        .arg("--data-dir")
        .arg(&data)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("Needle is not initialized; run `needle enable`")
    );
    assert!(!data.join("needle.sqlite3").exists());
}

#[test]
fn direct_explore_accepts_the_canonical_surface_without_query() {
    let output = Command::new(env!("CARGO_BIN_EXE_needle"))
        .args([
            "explore",
            "--route",
            "trace.state-flow",
            "--subject-kind",
            "behavior",
            "--subject",
            "activation",
            "--repository",
        ])
        .arg(repository_root())
        .arg("--data-dir")
        .arg(temporary_data("canonical"))
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("Needle is not initialized; run `needle enable`")
    );
}

#[test]
fn direct_explore_rejects_unknown_routes_before_running() {
    let output = Command::new(env!("CARGO_BIN_EXE_needle"))
        .args(["explore", "--route", "unknown", "--query", "Trace activation"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unsupported exploration route"));
}

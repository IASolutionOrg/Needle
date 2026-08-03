use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn invalid_hook_stdin_fails_open_with_diagnostic() {
    let binary = env!("CARGO_BIN_EXE_needle");
    let mut child = Command::new(binary)
        .args(["hook", "stop"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hook");
    child.stdin.take().unwrap().write_all(b"not-json\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "{}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid hook stdin"));
}

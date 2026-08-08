use std::path::PathBuf;
use std::process::Command;

fn needle() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_needle"))
}

#[test]
fn uninstall_help_documents_preserved_product_data() {
    let output = Command::new(needle()).args(["uninstall", "--help"]).output().unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Usage: needle uninstall"));
    assert!(stdout.contains("Product data is preserved"));
}

#[test]
fn uninstall_argument_surface_is_closed() {
    let output = Command::new(needle()).args(["uninstall", "--purge-data"]).output().unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("unknown uninstall argument `--purge-data`"));
}

#[cfg(windows)]
#[test]
fn development_binary_cannot_uninstall_its_build_directory() {
    let executable = needle();
    let output = Command::new(&executable).arg("uninstall").output().unwrap();

    assert!(!output.status.success());
    assert!(executable.is_file());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("not inside a complete managed Needle installation"));
}

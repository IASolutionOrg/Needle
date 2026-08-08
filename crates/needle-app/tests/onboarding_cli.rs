use needle_core::RoleProfileId;
use needle_runtime::RuntimeStore;
use serde_json::Value;
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn needle() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_needle"))
}

struct Fixture {
    root: PathBuf,
    bin: PathBuf,
    data: PathBuf,
    repository: PathBuf,
    codex: PathBuf,
    path: std::ffi::OsString,
}

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let root = env::temp_dir().join(format!("needle-onboarding-cli-{nonce}"));
        let bin = root.join("bin");
        let data = root.join("data");
        for directory in [&bin, &data, &root.join("home"), &root.join("local-app-data")] {
            fs::create_dir_all(directory).unwrap();
        }

        let codex = bin.join(if cfg!(windows) { "codex.exe" } else { "codex" });
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("onboarding_codex.rs");
        let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let output = Command::new(rustc)
            .args(["--edition=2024"])
            .arg(&source)
            .arg("-o")
            .arg(&codex)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "fixture compilation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let helper_names: &[&str] = if cfg!(windows) { &["curl.exe"] } else { &["curl"] };
        for name in helper_names {
            fs::copy(&codex, bin.join(name)).unwrap();
        }

        let mut paths = vec![bin.clone()];
        paths.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));
        let path = env::join_paths(paths).unwrap();
        let repository =
            fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap();
        Self { root, bin, data, repository, codex, path }
    }

    fn command(&self) -> Command {
        let home = self.root.join("home");
        let mut command = Command::new(needle());
        command
            .current_dir(&self.bin)
            .env("PATH", &self.path)
            .env("NO_COLOR", "1")
            .env("NEEDLE_DATA_DIR", &self.data)
            .env("CODEX_HOME", home.join(".codex"))
            .env("USERPROFILE", &home)
            .env("HOME", &home)
            .env("LOCALAPPDATA", self.root.join("local-app-data"))
            .env("XDG_DATA_HOME", self.root.join("xdg-data"));
        command
    }

    fn run(&self, arguments: &[&str]) -> Output {
        self.command().args(arguments).output().unwrap()
    }

    fn run_json(&self, arguments: &[&str]) -> Value {
        let output = self.run(arguments);
        assert!(
            output.status.success(),
            "{} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn common_arguments(&self) -> [String; 6] {
        [
            "--repository".to_owned(),
            self.repository.display().to_string(),
            "--data-dir".to_owned(),
            self.data.display().to_string(),
            "--json".to_owned(),
            "--no-color".to_owned(),
        ]
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if self.root.exists()
            && let Err(error) = fs::remove_dir_all(&self.root)
        {
            eprintln!("failed to remove test fixture {}: {error}", self.root.display());
        }
    }
}

#[test]
fn companion_commands_are_process_level_idempotent_persistent_and_bounded() {
    let fixture = Fixture::new();
    let common = fixture.common_arguments();
    let enable_arguments = [
        "enable".to_owned(),
        common[0].clone(),
        common[1].clone(),
        common[2].clone(),
        common[3].clone(),
        "--codex".to_owned(),
        fixture.codex.display().to_string(),
        "--worker-model".to_owned(),
        "gpt-test".to_owned(),
        common[4].clone(),
        common[5].clone(),
    ];
    let enable_refs = enable_arguments.iter().map(String::as_str).collect::<Vec<_>>();
    let first = fixture.run_json(&enable_refs);
    let second = fixture.run_json(&enable_refs);
    assert_eq!(first["status"], "enabled");
    assert_eq!(first["activation"]["state_digest"], second["activation"]["state_digest"]);
    assert_eq!(first["effective"]["enabled"], true);

    let status_arguments = [
        "status",
        common[0].as_str(),
        common[1].as_str(),
        common[2].as_str(),
        common[3].as_str(),
        common[4].as_str(),
        common[5].as_str(),
    ];
    let enabled_status = fixture.run_json(&status_arguments);
    assert_eq!(enabled_status["initialized"], true);
    assert_eq!(enabled_status["enabled"], true);
    assert_eq!(enabled_status["codex"]["isolation_verified"], true);

    let store = RuntimeStore::new(fixture.data.join("needle.sqlite3"));
    let profile_id = RoleProfileId::new("explorer.default").unwrap();
    assert_eq!(store.list_role_profile_revisions(&profile_id).unwrap().len(), 1);
    assert!(store.activation_status(&fixture.repository).unwrap().enabled);

    let ui_help = fixture.run(&["ui", "--help"]);
    assert!(ui_help.status.success());
    assert!(ui_help.stderr.is_empty());
    assert!(String::from_utf8(ui_help.stdout).unwrap().contains("Usage: needle ui"));
    let ui_invalid = fixture.run(&["ui", "--json"]);
    assert!(!ui_invalid.status.success());
    assert!(ui_invalid.stdout.is_empty());
    assert!(String::from_utf8(ui_invalid.stderr).unwrap().contains("unknown ui argument `--json`"));

    let stdout_path = fixture.root.join("serve.stdout");
    let stderr_path = fixture.root.join("serve.stderr");
    let mut server = fixture.command();
    let mut child = server
        .args([
            "serve",
            "--repository",
            &fixture.repository.display().to_string(),
            "--data-dir",
            &fixture.data.display().to_string(),
        ])
        .stdout(Stdio::from(fs::File::create(&stdout_path).unwrap()))
        .stderr(Stdio::from(fs::File::create(&stderr_path).unwrap()))
        .spawn()
        .unwrap();
    let started = Instant::now();
    let launch_url = loop {
        let output = fs::read_to_string(&stdout_path).unwrap();
        if let Some(url) =
            output.lines().find_map(|line| line.strip_prefix("Needle control plane: "))
        {
            break url.to_owned();
        }
        assert!(started.elapsed() < Duration::from_secs(15), "server did not report a launch URL");
        if let Some(status) = child.try_wait().unwrap() {
            panic!(
                "server exited before startup ({status}): {}",
                fs::read_to_string(&stderr_path).unwrap()
            );
        }
        thread::sleep(Duration::from_millis(25));
    };
    let authority =
        launch_url.strip_prefix("http://").and_then(|value| value.split('/').next()).unwrap();
    let mut connection = TcpStream::connect(authority).unwrap();
    connection.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(connection, "GET /health HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    connection.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"));
    assert!(response.contains(r#""schema":"needle.runtime-health/1""#));
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(fs::read_to_string(&stderr_path).unwrap().is_empty());
    assert!(store.activation_status(&fixture.repository).unwrap().enabled);

    let disable_arguments = [
        "disable",
        common[0].as_str(),
        common[1].as_str(),
        common[2].as_str(),
        common[3].as_str(),
        common[4].as_str(),
        common[5].as_str(),
    ];
    let disabled = fixture.run_json(&disable_arguments);
    assert_eq!(disabled["status"], "disabled");
    assert_eq!(disabled["effective"]["enabled"], false);
    assert!(!store.activation_status(&fixture.repository).unwrap().enabled);
    let disabled_status = fixture.run_json(&status_arguments);
    assert_eq!(disabled_status["initialized"], true);
    assert_eq!(disabled_status["enabled"], false);
}

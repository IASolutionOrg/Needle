use needle_bench::{RIPGREP_CALIBRATION_SHA, run_positive_calibration_replay};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    if let Err(error) = run() {
        eprintln!("proof-calibration-replay: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    let workspace = option_path(&arguments, "--workspace-root")
        .unwrap_or(env::current_dir()?)
        .canonicalize()?;
    let source_repository = option_path(&arguments, "--source-repository")
        .map(|path| absolute_from(&workspace, path))
        .unwrap_or_else(|| workspace.join("target/cache-pilot-v03-r15/repo"));
    let artifact_root = option_path(&arguments, "--artifact-root")
        .map(|path| absolute_from(&workspace, path))
        .unwrap_or_else(|| workspace.join("target/proof-calibration-v04-r23"));
    verify_source_repository(&source_repository)?;
    fs::create_dir_all(&artifact_root)?;
    let (report, worker_result) =
        run_positive_calibration_replay(&source_repository, &artifact_root)?;
    fs::write(
        artifact_root.join("worker-artifact-result-v2.json"),
        serde_json::to_vec_pretty(&worker_result)?,
    )?;
    fs::write(
        artifact_root.join("proof-calibration-report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    println!("{}", artifact_root.join("proof-calibration-report.json").display());
    Ok(())
}

fn verify_source_repository(repository: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let head = git_output(repository, &["rev-parse", "HEAD"])?;
    if head.trim() != RIPGREP_CALIBRATION_SHA {
        return Err(format!(
            "source repository HEAD is {}, expected {RIPGREP_CALIBRATION_SHA}",
            head.trim()
        )
        .into());
    }
    let status = git_output(repository, &["status", "--short"])?;
    if !status.trim().is_empty() {
        return Err("source repository is dirty".into());
    }
    Ok(())
}

fn git_output(repository: &Path, arguments: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
    let output = Command::new("git").arg("-C").arg(repository).args(arguments).output()?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn option_path(arguments: &[String], option: &str) -> Option<PathBuf> {
    arguments
        .iter()
        .position(|argument| argument == option)
        .and_then(|index| arguments.get(index + 1))
        .map(PathBuf::from)
}

fn absolute_from(workspace: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() { path } else { workspace.join(path) }
}

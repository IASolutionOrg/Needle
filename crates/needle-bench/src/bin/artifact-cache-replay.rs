use needle_bench::run_artifact_cache_replay;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    if let Err(error) = run() {
        eprintln!("artifact-cache-replay: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    let workspace = env::current_dir()?.canonicalize()?;
    let source_repository = option_path(&arguments, "--source-repository")
        .map(|path| absolute_from(&workspace, path))
        .unwrap_or_else(|| workspace.join("target/router-cache-source"));
    let artifact_root = option_path(&arguments, "--artifact-root")
        .map(|path| absolute_from(&workspace, path))
        .unwrap_or_else(|| workspace.join("target/artifact-cache-replay"));
    let report = run_artifact_cache_replay(&source_repository, &artifact_root)?;
    let report_path = artifact_root.join("artifact-cache-replay-report.json");
    fs::write(&report_path, serde_json::to_vec_pretty(&report)?)?;
    if !report.passed {
        return Err("artifact cache replay did not satisfy every gate".into());
    }
    println!("{}", report_path.display());
    Ok(())
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

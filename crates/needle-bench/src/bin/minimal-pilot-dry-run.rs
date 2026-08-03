use needle_bench::{run_minimal_pilot_dry_run, run_minimal_pilot_reworded_coverage_dry_run};
use std::fs;
use std::path::PathBuf;

fn main() {
    if let Err(error) = run() {
        eprintln!("minimal pilot dry-run failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut manifest = PathBuf::from("benchmarks/corpus/router-cache/manifest.json");
    let mut source_repository = PathBuf::from("target/router-cache-source");
    let mut artifact_root = PathBuf::from("target/minimal-pilot-dry-run");
    let mut output = None;
    let mut hit_mode = "exact".to_owned();
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        let value = arguments.next().ok_or_else(|| format!("{argument} requires a value"))?;
        match argument.as_str() {
            "--manifest" => manifest = PathBuf::from(value),
            "--source-repository" => source_repository = PathBuf::from(value),
            "--artifact-root" => artifact_root = PathBuf::from(value),
            "--output" => output = Some(PathBuf::from(value)),
            "--hit-mode" => hit_mode = value,
            _ => return Err(format!("unknown argument {argument}").into()),
        }
    }
    let report = match hit_mode.as_str() {
        "exact" => run_minimal_pilot_dry_run(&manifest, &source_repository, &artifact_root)?,
        "reworded-coverage" => run_minimal_pilot_reworded_coverage_dry_run(
            &manifest,
            &source_repository,
            &artifact_root,
        )?,
        _ => return Err(format!("unsupported --hit-mode {hit_mode}").into()),
    };
    let encoded = serde_json::to_string_pretty(&report)?;
    let output = output.unwrap_or_else(|| artifact_root.join("report.json"));
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&output, format!("{encoded}\n"))?;
    println!("{encoded}");
    eprintln!("minimal pilot dry-run report written to {}", output.display());
    Ok(())
}

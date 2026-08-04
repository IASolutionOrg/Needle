use needle_bench::{
    CorpusPreflightOptions, FrozenCorpusManifest, MAX_CORPUS_MANIFEST_BYTES,
    preflight_exit_is_failure, preflight_frozen_corpus_with_options, read_bounded_file,
};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    if let Err(error) = run() {
        eprintln!("corpus-preflight: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    let workspace = option_path(&arguments, "--workspace-root")
        .unwrap_or(env::current_dir()?)
        .canonicalize()?;
    if let Some(path) = option_path(&arguments, "--digest") {
        let path = absolute_from(&workspace, path);
        println!(
            "b3:{}",
            blake3::hash(&read_bounded_file(&path, MAX_CORPUS_MANIFEST_BYTES)?).to_hex()
        );
        return Ok(());
    }
    let manifest_path = option_path(&arguments, "--manifest")
        .map(|path| absolute_from(&workspace, path))
        .unwrap_or_else(|| workspace.join("benchmarks/corpus/router-cache/manifest.json"));
    let source_repository = option_path(&arguments, "--source-repository")
        .map(|path| absolute_from(&workspace, path))
        .unwrap_or_else(|| workspace.join("target/router-cache-source"));
    let output = option_path(&arguments, "--output")
        .map(|path| absolute_from(&workspace, path))
        .unwrap_or_else(|| workspace.join("target/corpus-preflight/report.json"));
    let execute_focused_tests =
        arguments.iter().any(|argument| argument == "--execute-focused-tests");
    let options = CorpusPreflightOptions {
        schedule_path: option_path(&arguments, "--schedule")
            .map(|path| absolute_from(&workspace, path)),
        power_plan_path: option_path(&arguments, "--power-plan")
            .map(|path| absolute_from(&workspace, path)),
        sealed_bundle_index_path: option_path(&arguments, "--sealed-bundle-index")
            .map(|path| absolute_from(&workspace, path)),
        sealed_bundle_root: option_path(&arguments, "--sealed-bundle-root")
            .map(|path| absolute_from(&workspace, path)),
    };
    let manifest_bytes = read_bounded_file(&manifest_path, MAX_CORPUS_MANIFEST_BYTES)?;
    let manifest: FrozenCorpusManifest = serde_json::from_slice(&manifest_bytes)?;
    let manifest_directory =
        manifest_path.parent().ok_or("manifest path must have a parent directory")?;
    let report = preflight_frozen_corpus_with_options(
        &manifest,
        manifest_directory,
        &source_repository,
        execute_focused_tests,
        &options,
    );
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&output, serde_json::to_vec_pretty(&report)?)?;
    println!("{}", output.display());
    // Preflight is an offline diagnostic.  Missing production-only material
    // is reported in bounded fields and intentionally does not turn the
    // command into a provider attempt or a shell-level failure.
    if preflight_exit_is_failure(&report, &options, execute_focused_tests) {
        return Err("corpus preflight failed closed".into());
    }
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
